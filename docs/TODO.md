# TODO

## personal agent v1（wayfinder 建图中，2026-10-31）

- [x] **能力调研** — 行业一手资料 + 代码核对的缺口分析：`.scratch/personal-agent-capabilities/research.md`
- [x] **Wayfinder map** — 五件底座（信任边界、记忆写侧、MCP + 一个真实集成、一个 IM 通道、可靠性）的决策地图与 15 张票（票 15 上下文压力由票 05 毕业新增）：`.scratch/personal-agent-capabilities/map.md`；目的地是 `spec.md` + ADR，落稿后交 `/implement`
- [x] **WebUI 认证与网络边界** — 纯本机入口、无认证、只监听 loopback + 四条浏览器侧中间件；已回填票 02，决策见 `docs/adr/0010`
- [x] **工具风险与审批策略** — 5 类风险级 → 3 档管控；审批以回合结束 + 下一轮兑现，无 `ask_user` 工具；已回填票 03，决策见 `docs/adr/0011`
- [x] **记忆写侧语义** — 单文件漏斗（主模型只用 file 工具写 MEMORY.md）+ 分类/晋升归 Dream + Protected document 代码强制；已回填票 05，决策见 `docs/adr/0012`；连带判定 `Daily` v1 不建设、history 不进检索语料
- [x] **上下文压力（Context pressure）** — 单一窗口锚 + 占比派生全部长度限制；清理与可找回配对（引用带落盘路径、必要时补落盘）；豁免改为工具自述 `context_exempt()`；已回填票 15，决策见 `docs/adr/0013`；连带两条 breaking change（废弃 `agent.max_tool_result_chars`、`context_window_tokens` 移到 provider 段）
- [x] **记忆生命周期** — 把已做判断的地方接上容量（不新增 flush、不多新机件）：summary 预算 512 字节→窗口 2% 占比、history 只删已处理连续前缀、Dream 接 trace + 指纹级审计（不建独立审计文件）、改读真实配置（两锚分离）、回滚靠写前快照（`memory/.snapshots/` 分层轮转、不用 git）、条目不写日期、年龄由快照序列 diff 反推、成功判定换结构判定；已回填票 06，决策见 `docs/adr/0014`；回溯修订票 05 provenance 约定与票 15 文档预算锚归属
- [x] **研究票：密钥与 OAuth 凭证管理** — 结论写入 `.scratch/personal-agent-capabilities/research-secrets-oauth.md`，已回填票 04；keychain 优先 / env 次之 / 0600 配置兼容
- [x] **单用户身份模型** — 显式单例 owner；共享记忆 / 分通道会话；群聊只有 owner 可授权；结论回填票 01
- [x] **触发器设计（wayfinder 已收官，2026-10-31）** — 七张决策票全部 resolved，spec 与 ADR 已落稿：`.scratch/personal-agent-triggers/spec.md` + `docs/adr/0006..0009`；ADR 篇目：subagent 运行时形状、触发器是 cron 之上的一层、codemode 运行时选型、外部内容不进上下文
  - [x] 01 subagent 机制 [x] 02 采集与快照 [x] 03 判定语义
  - [x] 04 执行与投递 [x] 05 codemode 运行时 [x] 06 时间源
  - [x] 07 提示词/skill/文档 [x] 08 spec 落稿
- [ ] **Frontier（下一个 map）** — 检索与存储（票 07）、MCP 客户端（票 08）、首个 IM 通道（票 09）、出站投递（票 10）、可靠性预算（票 11）、触发器接缝（票 12）、v1 差异化（票 13）、spec 落稿（票 14）；heartbeat 与触发器的长期边界、群聊 origin 归属已随本 map 移出范围

## 记忆生命周期实现切片（票 06 裁决，2026-10-31）

决策见票 06 + `docs/adr/0014`；实现待 spec 落稿后展开。本票发现的实现级缺陷（先于新功能修）：

- [ ] **两个 panic 修复** — `dream.rs` `build_prompt` 的 `&e.content[..500]` 是字节切片，中文非 char boundary 会 panic 导致 cursor 不推进、每 tick 重跑同一批再崩（永久卡死）；`consolidate.rs` summary 保头截断必须走 `is_char_boundary` 循环（同仓库已有参考）
- [ ] **cursor 归零重复整理** — `compact_history_after_dream` 的 `saturating_sub(deleted_count)` 在被删数大于游标时归 0，剩下条目被当全部未处理→重复写入；删除范围限定为“已处理的连续前缀”，cursor 删后仍指向第一条未处理
- [ ] **summary 预算改占比** — `MAX_SUMMARY_BYTES = 512` 改为按票 15 窗口 2% 占比派生（含绝对上下限）；`[skip]` 行进 `history.jsonl` 前在代码里过滤掉
- [ ] **Dream 接 trace + 改读真实配置** — 接上 `None` 的 tracer（session `system:dream`）；删掉 `AgentConfig::default()` 轻径，改读 provider/agent；两锚分离（截断类取自身窗口、文档预算取主 agent 窗口）
- [ ] **成功判定换结构判定** — 英文子串 `contains("error"/"failed")` 改为：run 正常结束（无 error finish_reason、未超 `max_iterations`）且是否有写入；cursor 推进与快照写成功同一判定分支
- [ ] **写前快照与分层轮转** — memory 工具改写前复制旧内容到 `memory/.snapshots/{run 时间戳}/{文档}`（只含本轮真动过的）；最近 30 天全留、之后每篇每日一、更久每月一；清理挂 Dream 成功分支（与 `tool-results/` TTL 同点）
- [ ] **年龄反推与标记清洗** — 逐快照 diff 算出正文行首次出现版本→只附进 Dream 提示侧信道（标题行不计年龄、缺档标未知不外推）；`memory_read` 零注解，读写两侧剔年龄标记（`memory_edit` 的 `old_string` **匹配前**就剔）；覆盖 `file_writer`/`file_editor` 写 MEMORY.md 路径；清洗动作记 trace

## personal agent 触发器（实现切片，2026-10-31 起）

Spec 见 `.scratch/personal-agent-triggers/spec.md`（实现阶段的唯一依据；文件路径与行号等易过时细节见同目录 `issues/08-spec-assembly.md` 的 `## Comments` 存档）；决策见 `docs/adr/0006..0009`。实现票 10–14 在 `.scratch/personal-agent-triggers/issues/`：10 与 11 可并行，12 依赖两者，13 依赖 12，14 依赖 10 与 13。

- [ ] **10 subagent 机制**（票 10）— `src/agent/`：`agents/*.md` 扫描与校验、per-run `ToolManager`、`RunRegistry` + 并发闸、`subagent` 工具、`# Available Agents` 注入、`[Subagent result]` 经 `publish_inbound` 回 origin、`drop_session` 与不落盘、`subagent.run` span
- [ ] **11 codemode 沙箱**（票 11）— `src/codemode/`：rquickjs 执行层（专职线程 + LocalSet）、宿主函数注册、双层超时、内存上限、输出截断、secret 脱敏
- [ ] **12 源采集与 shape**（票 12）— `src/trigger/source/` + `trigger_source` 工具 + tick 骨架 + 内置 `trigger` agent：目录扫描与热加载、`collect.js` 归一化、`snapshot`/`append` 去重、ring + TTL、freeze/commit、到期判定与并发 4、失败降频
- [ ] **13 判定、状态机与试跑**（票 13）— `src/trigger/subscription/` + `trigger` 的 list/create/update/delete/test：`judge.js` 契约与返回校验、边沿 + cooldown 状态机、ready 门槛、`schedule` 闸门、`state.json` 原子读写、失败自愈与告警计数
- [ ] **14 执行、投递与历史**（票 14）— `src/trigger/dispatch/` + gateway 装配：`DispatchQueue`、`agent`/`notify` action、`deliver` 三级求值、`notify` 同 tick 合并、失败告警、`history.jsonl`、三个 root span、`docs/trigger.md`

## WebUI json-render 卡片（2026-10-24 完成）

- [x] **run_id 贯通（T01）** — 恒为空串的 `BusResult.task_id` 落实并改名 `run_id`，从 `TaskHook` 分配，贯穿生命周期事件、`BusResult` 与卡片事件
- [x] **SpecCompiler + transcript 裁剪（T02）** — `src/card/`：JSONL patch 流式编译为 Card spec（`root` + `elements` + `state`），坏行跳过/超限熔断；Run transcript 按消息游标裁剪（`TaskStarted` 记游标、`TaskCompleted` 读 `messages[cursor..]`）
- [x] **首张卡片端到端（T03）** — `CardService`（channel 层、Run 后独立流式调用）、Catalog 注册端点 `POST /json-render/catalog`（fail-closed 能力握手）、`Channel::write_card` 默认 no-op + 独立 mpsc 经 `ChannelManager` 路由、前端 index.html 卡片渲染（不污染会话）
- [x] **卡片流式铺开（T04）** — `Provider::chat_stream`（trait 默认降级为单次 delta）+ OpenAI 真流式 SSE（`stream:true` + `include_usage`、`SseLineReader` 跨 chunk 缓冲、坏行跳过、带 tools 回落非流式）；`CardEvent::Started/Delta/Completed/Failed`；前端 rAF 合并增量渲染
- [x] **词表扩充与卡片内交互（T05）** — 前端 registry 扩到十二种组件；卡片内交互限本地状态（`setState`/`visible`），动作不上行
- [x] **卡片回放（T06）** — Card sidecar `sessions/{chat_id}.cards.jsonl`（以 Run 末条 assistant 消息 timestamp 为锚，不进会话 JSONL）；`/session` 返回契约扩 `ts`/`card` 字段，按锚点精确挂回，锚点消失则不挂
- [x] **护栏与配置（T07）** — `CardPolicy`（并发闸门 max 2 超限跳过、30s 超时、进程退出取消进行中生成）；`json_render.generate` span（`json_render.patch_count` + token usage，失败路径 Error status）；顶层 `json_render` 配置段（enabled/model_override/max_tokens/timeout_seconds，入 config schema + patch）；卡片 provider 解析（override 命中或回退主 provider，预算恒为 json_render.max_tokens）
- [x] **文档收口（T08）** — docs/channels.md 卡片下发节 + 端点表、docs/channel.md `write_card`/卡片路由、docs/provider.md 重写为实际 trait（含流式契约）、docs/config.md `json_render` 段、CONTEXT.md（现为 GLOSSARY.md）术语与 ADR 0005
- [x] **验证** — `cargo fmt -- --check`、`cargo clippy --all-targets`（无 warning）、`cargo test` 全过（1181 lib + 全部集成）
- [x] **上线后修复：无进展超时窗口（2026-10-24）** — 旧语义下 `timeout_seconds` 是单次调用总时长上限，思考模型（如 qwen3 推理阶段只发 `reasoning_content` chunk）必被误杀。`Provider::chat_stream` 回调从 `FnMut(String)` 升级为 `FnMut(StreamSignal)`（`Content` 内容增量 / `Activity` 传输层存活心跳）；OpenAI 实现把合法但无内容的 chunk（role 开场、reasoning、usage、`[DONE]`）映射为 `Activity`，坏行不发信号；`CardService::generate` 改为看门狗结构（provider 调用入 worker task，每信号续命，biased select 监督 done/tick/deadline，停滞或取消时由 `AbortOnDrop` 掐断 worker），窗口内毫无信号才判停滞。降级（非流式）provider 行为不变：调用完成前零信号，表现仍为总时长上限。回归测试：`test_stream_activity_signals_extend_the_timeout_window`、`test_stream_stall_after_progress_still_fails`
- [x] **上线后修复：卡片调用禁用思考（2026-10-24）** — `ProviderConfig` 新增 `disable_thinking` 字段（true 时非流式/流式请求体都注入 `enable_thinking: false`，DashScope 兼容接口扩展字段；未标记的 provider 不发该字段）；`card_provider_config` 对卡片 provider 强制置 true（机械的转录→JSONL 转换不需要推理，思考阶段白白拖慢首信号与总延迟），主 agent 对话不受影响。回归测试：`test_card_provider_disables_thinking_for_card_calls`、`test_chat_disable_thinking_injects_enable_thinking_false`、`test_chat_stream_disable_thinking_injects_enable_thinking_false`

Spec 见 `.scratch/webui-json-render-cards/spec.md`；票文见同目录 `issues/`；决策见 `docs/adr/0005-post-run-card-generation.md`。

**后续切片：**

- [ ] **卡片动作上行（emit）** — 卡片交互驱动新一轮对话：需单独输入通道、防重入、「用户打字 vs 卡片点击」区分（ADR 0005 已标注另立 ADR）
- [ ] **多套 Catalog** — 当前每 chat 一份上报的 Catalog prompt；多客户端词表共存（不同版本 registry / 不同主题词表）与服务端选套策略待立项
- [ ] **侧车清理策略** — `*.cards.jsonl` 只追加无回收：会话删除/consolidation 后锚点失效的卡片清理、体积上限、与 session 生命周期对齐均未实现

## Memory entry 索引与检索（只读切片，2026-09-26 完成）

- [x] **ADR 0004** — 记录 entry 层是文档的派生视图、有意偏离 zeroclaw 的 entry 权威模型、关键词优先、只读、ADR 0003 继续有效（叠加关系）
- [x] **prefactor** — `SkillMeta` / `parse_skill_frontmatter` 从 `context.rs` 下沉到 `src/memory/frontmatter.rs`，available-skills / active-skills 注入行为逐字不变（由既有 context 集成测试锁定）
- [x] **MemoryIndex trait + KeywordIndex** — `src/memory/index.rs`：段落解析（空行边界、ATX heading、fence、frontmatter 剥除）、分词（拉丁 + CJK bigram + 单字 unigram 回退）、BM25-lite 打分（k1=1.2、b=0.75、heading ×2）、确定性 tie-break（分数→category→source→heading→hash）
- [x] **MemoryService::search** — `&mut self` 惰性索引 + 内容指纹失效重建；覆盖 5 个固定文档 + skills + customs，全部经 backend 读取；索引永不落盘
- [x] **memory_search 工具** — `query` 必填、`category` 过滤器、`limit` 缺省 10 / 上限 50 / 非法值报错；输出带 category、heading、source；无匹配明确文案；只读
- [x] **注册** — `AgentLoop::from_parts` 注册，CLI 与 gateway 两种模式都可用（与 `message` / `cron` 一样不进 `config.tools`）
- [x] **测试** — index 单测（分词 / 解析 / 排序 / 稳定性 / CJK）+ facade 主缝（重建、编辑、删除、新增、指纹复用计数、不落盘、过滤、上限）+ 工具契约（schema / 过滤 / 钳制 / 非法值 / 无匹配 / 只读）+ agent_loop 注册
- [x] **文档** — `docs/memory.md` 增补 Memory entry / Memory index / 分词与打分 / `memory_search` 小节，并指向 ADR 0004

Spec 见 `.scratch/memory-entry-index/spec.md`。

**后续切片：**

- [ ] **切片 2：配置与查询增强** — `memory.search.*` 开关（如需）、前缀与短语查询；~~Daily category 语义与每日日志写入方~~（personal agent v1 票 05 已判定：`Daily` 无写者、不进检索语料，v1 不建设）
- [ ] **切片 3：embedding / hybrid** — 引入 `SearchMode`，以 embedding provider 替换或融合 `KeywordIndex`
- [ ] **切片 4：写侧** — 已由 personal agent v1 票 05 定案并改写：~~`store` / `forget` / `supersede` / dedup gate / 冲突取代~~ 均不新增工具，主模型用现有 `file_writer`/`file_editor` 写 `memory/MEMORY.md`，分类与晋升归 Dream（见 `docs/adr/0012`）；~~把 `hash` 换成稳定哈希（如 sha2）~~ v1 不做（条目身份是标题/正文，跳进程稳定 id 无消费者）。待做：写盘改 `write_file_atomic`、指纹乐观并发校验、Protected document 拒写、`gateway.dream.doc_budget.*` 预算与超限置标记（`retention` / 时间衰减仍待票 06、票 15）

## Memory 模块统一：category 化 + backend 抽象（2026-09-26）

- [x] **类型层** — `MemoryCategory` 全单元枚举（Soul/User/Agent/Tools/Core/Daily/Skill/Custom）+ `MemoryBackend` trait + `EditOutcome`
- [x] **MarkdownBackend + factory** — 按 category 映射文件路径（保持既有路径契约）、`create_backend` / `backend_from_name`
- [x] **HistoryStore** — `history.jsonl` + Dream 游标 + 压缩，独立于 backend trait
- [x] **MemoryService facade** — category 操作委托 backend；`get_effective` 承接「未修改模板不注入」
- [x] **配置** — 顶层 `memory` section，`memory.backend = "markdown"`（白名单，预留 sqlite）
- [x] **调用点迁移** — 全仓库改用 `MemoryService` / `SharedMemory` + category API
- [x] **context 按 category 注入** — Agent→Soul→Tools→User→skills→Core→history（skills 经 `list_skill_names` + get）
- [x] **bootstrap/setup 收窄** — 只写 5 个记忆文件 + skills；identity/dream 仅嵌入不落盘
- [x] **Dream 改造** — 工具收敛为 `memory_read` / `memory_write` / `memory_edit`（白名单 soul/user/core/skill），移除全部文件工具；dream.md 重写
- [x] **删除兼容层** — 移除 `MemoryStore` / `SharedMemoryStore` 别名与旧同步方法
- [x] **文档 + ADR** — docs/memory.md 重写，bootstrap/context/index 更新，CONTEXT.md（现为 GLOSSARY.md）术语，ADR 0003
- [ ] **后续：sqlite backend** — 实现第二个 `MemoryBackend`，接线 `memory.backend` 白名单
- [x] **entry 级只读检索** — 在 backend trait 之上叠加派生 entry 层（见 ADR 0004；写侧仍待切片 4）
- [ ] **后续：entry 写侧 API** — 若出现逐条目去重 / 语义召回需求，在 entry 层之上增加写侧（见上方切片 4）

## Trace chat span 增加 response content attribute（2026-08-16）

- [x] **chat span 记录 response content** — `src/runner/mod.rs` 在收到 provider 响应后为 chat span 设置 `gen_ai.response.content`（string，完整 content 不截断，与 `http.request.body` 的「全量不截断」决策一致）；无 content 的纯工具调用响应不设置该 attribute
- [x] **测试** — `test_runner_chat_span_records_response_content`（多轮迭代各携带自己的 content）、`test_runner_chat_span_omits_content_when_absent`（纯工具调用响应不设置）
- [x] **验证** — `cargo fmt -- --check`、`cargo clippy`（无新 warning）、`cargo test` 全过

## Gateway 入站任务缺失 tracer（2026-08-16 修复）

- [x] **根因** — `AgentLoop.start_inbound`（Gateway WebUI/cron/heartbeat 任务路径）经 `SessionTaskBuilder` 构建 runner，但 builder 从未携带 tracer → 入站任务全部无埋点，`traces.jsonl` 保持空文件（CLI 模式走 `run_task`/`self.runner` 正常）。证据：Gateway 启动时 `reset_trace_files` 创建了空 `traces.jsonl`（17:53:00），同一进程内 WebUI 任务完成但零 span 落盘
- [x] **修复** — `SessionTaskBuilder` 新增 `tracer` 字段 + `.tracer()` builder 方法并在 `build()` 中传入 `AgentRunner`；`AgentLoop` 增加 `tracer` 字段（`from_parts` 中与 runner 共享同一实例），`start_inbound` 的普通任务与 `/stop` sentinel 任务均传入；dream 保持显式不追踪（原设计）
- [x] **回归测试** — `test_gateway_inbound_task_writes_trace_spans`：tracing 开启 + file backend（空 path）→ `from_parts` → `start_inbound` → 经 message_bus 发送任务 → 断言 `{workspace}/traces.jsonl` 出现 span 行（修复前 RED：文件为空；修复后 GREEN）
- [x] **验证** — `cargo fmt -- --check`、`cargo clippy`（无新 warning）、`cargo test` 全过（1137 lib + 集成，4 连跑稳定）

## Trace viewer 详情路由 404（2026-08-16 修复）

- [x] **根因** — `build_router` 中详情路由写作 `/traces/api/trace/{id}`，但项目依赖 axum 0.7（matchit 0.7），其捕获语法是 `:id`；`{id}` 是 axum 0.8+ 语法，在 0.7 下会被注册为**字面量**路径段 `{id}`，导致列表页能列出 trace（`/traces/api/traces` 正常），展开详情时 `/traces/api/trace/<full_trace_id>` 永不匹配 → 404 → 前端显示 "Trace not found (maybe still in progress)." 证据：同一数据直调 handler 返回 200，经路由即 404；echo 路由实验证明 `{id}` 不匹配、`:id` 匹配
- [x] **修复** — 路由改为 `/traces/api/trace/:id`；同时把 `start_server` 中的路由表提取为 `build_router()`，使 HTTP 层可测（原 handler 直调测试绕过路由匹配，无法捕获此类 bug）
- [x] **回归测试** — `test_webui_trace_detail_route_matches_full_trace_id`：真实 HTTP 请求经 `build_router` 打到详情端点，断言 200 + `full_trace_id` 回显（修复前 RED：404；修复后 GREEN）
- [x] **验证** — `cargo fmt -- --check`、`cargo clippy`（0 warning）、`cargo test` 全过（990 lib + 集成）；用真实 `~/.slimbot/workspace/traces.jsonl` 复跑原复现循环，详情请求 404 → 200

## Trace viewer 详情渲染崩溃（2026-08-16 修复）

- [x] **根因** — `SpanNode` 用 `#[serde(flatten)]` 序列化（span 字段直接内联到树节点，无 `span` 包装），但前端 `renderNode` 读取 `node.span` → `undefined` → 展开详情时 JS 崩溃 "undefined is not an object (evaluating 'span.span_id')"。Rust 侧序列化测试 `test_span_node_serializes_attributes` 已锁定 flatten 形状，前端是偏离方
- [x] **修复** — `resources/webui/traces.html` 的 `renderNode` 改为 `const span = node`（节点本身即 span + children），与 in-flight 扁平路径 `renderFlatSpan` 的读取方式统一
- [x] **回归测试** — `test_detail_wire_shape_matches_trace_viewer_frontend`：锁定 detail 载荷线契约（root/children 均为裸 span 字段、无 `span` 包装），防止序列化变更再次静默破坏前端
- [x] **验证** — 用真实 `traces.jsonl` 的 detail 载荷跑真实前端渲染（node 抽取 `traces.html` 脚本）：修复前 CRASH（`Cannot read properties of undefined (reading 'span_id')`），修复后渲染 3 行且全部 span_id 存在；`cargo fmt -- --check`、`cargo clippy`（0 warning）、`cargo test` 全过（1139）

## Review 修复（2026-08-16，code review 子代理发现）

- [x] **`/stop` sentinel 对已存在会话 panic（既有 bug）** — `handle_agent_loop_command` 的 `/stop` 分支经 `SessionTaskBuilder` 建任务时只传了 session_manager/outbound_tx/cancel_token/max_tokens，`build()` 对缺失字段 `.expect(...)` → 任何已存在会话的 `/stop` 都会 panic（`tool_manager required`）。修复：`handle_agent_loop_command` 增加 tool_manager/provider/config/workspace_dir/memory_service 参数并完整构建 sentinel；回归测试 `test_stop_sentinel_with_existing_session_runs`（修复前 RED：inbound loop 被 panic 杀死，超时）
- [x] **全局 WorkerPool 跨 runtime 复用死 worker（既有 bug，测试暴露）** — `WorkerPool` 是进程级 OnceLock 单例，但 worker 的 loop 是 `tokio::spawn` 在创建它的 runtime 上；并行测试中某 runtime 结束 → 其 worker 仍被后续 submit 当作空闲复用 → 任务永久搁浅（生产单 runtime 不受影响，测试多 runtime 触发）。修复：worker 记录 `runtime_id`，submit 只复用当前 runtime 的 worker（否则新建，上限内）；loop 加 `AliveGuard`（future 被取消/退出时置 dead）并在 submit/reap 时剔除死 worker；回归测试 `test_pool_skips_dead_workers_from_finished_runtime`
- [x] **注释准确性** — `AgentLoop.tracer` 字段注释修正：仅 WebUI 入站请求与 `/stop` sentinel 走 `SessionTaskBuilder`，cron/heartbeat 复用 `self.runner`

## WebUI 配置编辑器（2026-08-16 完成）

- [x] **config 定义扩展** — `define_config!` 新增 `secret` 约束（`FieldMeta.secret`）；section 改为字符串字面量（支持 `gateway.cron` 等点分路径）；修复 `allowed_max` 元数据派发；迁移 `CronConfig`/`HeartbeatConfig`/`DreamConfig`/`WebFetchConfig` 进 `config_defs.rs`；`ProviderConfig.api_key` 标 `secret`；`Config::clamp()` 补调新结构体
- [x] **schema 序列化** — 新增 `src/config_schema.rs`：宏结构体 + 集合 → schema 树（kind 派生、secret 掩码、`r#type` 归一化）；10 个 section（agent/gateway.cron/heartbeat/dream/web_fetch/tracing + providers/tools/channels/tracing.backends）
- [x] **patch 引擎** — `apply_config_patch`（set/add/remove 按 dot-path），应用后 clamp→normalize→validate，失败返回逐字段错误不写盘；非法 path/类型错误返回 Err
- [x] **WebUI 接线** — `config_path` 穿入 `register_webui_factory`→`WebuiChannelFactory`→`WebuiChannel`→`AppState`；新增 `GET /config`、`GET /config/api/state`（schema + 密钥掩码）、`POST /config/api/update`（patch 应用 + 写回）；handler 为薄壳
- [x] **前端** — `resources/webui/config.html`：按 schema kind 动态渲染表单、集合 CRUD（providers/backends 结构化、tools/channels 开关+裸 JSON）、api_key 掩码 + 留空不变、逐字段错误高亮 + 放弃重载、顶部重启生效提示、index/traces/config 双向互链
- [x] **文档** — `docs/config.md` 增补「WebUI 配置编辑器」章节；`docs/index.md` 修正陈旧 `config_scheme.md` 条目（删除该陈旧文档）；`CONTEXT.md`（现为 `GLOSSARY.md`）术语与 `docs/adr/0002` 已落盘
- [x] **验证** — `cargo fmt --check`、`cargo clippy`（无新 warning）、`cargo test` 全过（982 lib + 集成）

Spec 见 `.scratch/webui-config-editor/spec.md`。

## WebUI Trace 查看器（2026-08-16 完成）

- [x] **配置** — `TraceConfig` 增加 `reset_on_start`（默认 true，无范围/白名单约束，`clamp()` 不破坏）
- [x] **文件生命周期** — `reset_trace_files` 纯函数（true 截断 / false 保留 / 返回重置数）；Gateway 启动时调用（CLI 不触发，`run_cli_agent` 不调用）
- [x] **默认路径** — file backend `path` 为空时解析为 `{workspace_dir}/traces.jsonl`（共享 `resolve_trace_file_paths` 供 build_tracer 与 WebUI 复用）
- [x] **WebUI 接线** — `resolve_trace_file_paths` 结果注入 `WebuiChannelFactory` → `WebuiChannel` → `AppState`；无 file backend 时路径为空，查看器显示禁用态
- [x] **后端接口** — `GET /traces`（HTML）+ `/traces/api/traces`（列表，按开始时间降序，含 `startup_ms`/`disabled`/`too_large`）+ `/traces/api/trace/{id}`（span 树详情 + 扁平 spans，404 兜底）+ `/common.css`；解析/分组/排序/建树在 `src/trace_viewer.rs` 纯函数（可单测），handler 为薄壳；50 MiB 大小保护
- [x] **前端** — 抽公共 `webui/common.css`（index.html 视觉零变化）；新增 `resources/webui/traces.html`（列表 + 树形展开 + 大 JSON 折叠 + 超长截断 + 空态/禁用态 + 双向互链 + 启动时间/已运行时长 + 手动刷新/自动加载）
- [x] **文档** — `docs/trace.md` 截断语义与默认路径更新、CONTEXT.md（现为 GLOSSARY.md）增补 trace file / trace viewer 术语、ADR-0001 不涉及无需更新
- [x] **code review 修复** — `startup_ms` 由接线层记录真实 Gateway 启动截断时刻（替代文件 mtime，避免追加后失真）；`TraceDetail` 补充 `full_trace_id`；移除根 span 恒有 end 的不可达 running 分支；前端抽取 `renderSpanDetail` 去重、清理 `fmtTime`/`statusBadge` 死代码
- [x] **验证** — `cargo fmt --check`、`cargo clippy`（无新 warning）、`cargo test` 全过（941 lib + 集成）

Spec 见 `.scratch/webui-trace-viewer/spec.md`。

## Trace 机制（2026-08-15 完成）

- [x] **span 模型** — `src/trace/`：`SpanContext`（trace_id 16B / span_id 8B / parent_span_id）、`SpanGuard`（RAII：构造 start、Drop end、`set_status`/`set_attribute` 累积）、`SpanRecorder` trait
- [x] **Tracer + registry** — `Tracer`（`Option<Arc<Tracer>>` 显式 DI，零 backend 时 no-op）、fan-out 多路、每 backend 隔离不可失败
- [x] **TraceBackend trait** — `on_span_start`/`on_span_end`（同步 `&self`、不可失败）
- [x] **file backend** — 每 span 结束时同步 append 一行 JSONL（`parking_lot::Mutex<File>`）
- [x] **otel backend** — `opentelemetry`/`opentelemetry_sdk`/`opentelemetry-otlp`（`http-proto`），HTTP 导出默认 `http://localhost:4318`
- [x] **埋点** — 骨架 `run()` 开 `agent.run` 根 span、每轮 `iteration`、`provider.chat`/`tool.{name}` 子 span；span 单独创建不耦合 hook；provider.chat 经 `Option<&dyn SpanRecorder>` 记录完整 body
- [x] **config** — `tracing.enabled` + `backends` 数组（同 type 多实例），走 `define_config!`
- [x] **测试** — mock backend 断言 span 树/属性/状态；RAII Drop、fan-out、隔离、file JSONL、body 捕获、config 解析
- [x] **OTel GenAI 语义约定对齐** — span 名与 attribute 遵循 `gen-ai-agent-spans.md`/`gen-ai-spans.md`：根 `invoke_agent {agent}`（`gen_ai.operation.name`/`gen_ai.agent.name`/`gen_ai.conversation.id`/`gen_ai.conversation.compacted`/`gen_ai.usage.*`）、`chat`（`gen_ai.provider.name`/`gen_ai.request.model`/`gen_ai.usage.*`/`gen_ai.response.finish_reasons`）、`execute_tool {name}`（`gen_ai.tool.name`/`gen_ai.tool.call.id`/`gen_ai.tool.call.arguments`）；错误用低基数 `error.type`；otel backend 支持数组 attribute

设计文档见 `docs/trace.md`；决策见 `docs/adr/0001-self-built-span-model.md`。

## Runner Phase Hooks 重构（2026-08-15 完成）

- [x] **Phase Hook 管线** — `AgentRunner::run()` 重构为约 80 行骨架，七个 phase（PreTurn/PreChat/AfterChat/PreTool/AfterTool/PostIteration/AfterTurn）通过可插拔 hook 实现，`HookRegistry` 封装存储/执行/取消检查
- [x] **Hook 注册 API** — builder 支持 `with_defaults()` + 每 phase 的 `add_*` 方法，`HookPosition::{Append, Replace, Insert}` 控制位置（越界退化为 Append）
- [x] **流程控制** — `HookAction::{Continue, Abort}` / `ToolHookAction::{Execute, Skip, Abort}`，abort 跳转 after_turn，pre_tool 可跳过执行返回合成结果
- [x] **RunSpec/RunState** — 不可变配置 + 可变状态分离，`RunState` 提供类型安全 `set_extension`/`get_extension` 跨 phase 传数据
- [x] **默认 hook** — 14 个默认 hook 复刻原 run() 全部行为（`src/runner/hooks/` 每 hook 一文件），`AgentRunner::new()` 内部自动注册
- [x] **取消/回滚语义保留** — after_turn 始终执行（即使取消/出错），LLM 错误/取消回滚到 initial_message_count；PreTurn 阶段取消同样回滚（清除本轮已写入的用户消息，避免会话以未回复的 user 消息结尾）；chat 前增加取消检查，已取消时跳过 LLM 调用并回滚，不浪费请求
- [x] **测试** — 现有 36 个 runner 测试无修改通过；新增 HookRegistry 单测、每 hook 单测、9 个 hook 行为集成测试

设计文档见 `.scratch/runner-phase-hooks/spec.md`；模块文档见 `docs/runner.md`。

## 缺失工具（对齐 nanobot）

### 核心工具

- [x] **grep / glob** — 内容搜索和文件发现工具，支持 `output_mode`（`files_with_matches` / `count` / 默认带行号）、`glob` 过滤、`type` 过滤、`head_limit` / `offset` 分页、`context_before` / `context_after`
- [x] **web_fetch** — URL 内容抓取工具，使用 readability-rust + quick_html2md 提取可读内容为 markdown，支持 `extract_mode`（`markdown` / `text`）和 `max_chars` 参数，返回带元数据的 JSON
- [ ] **web_search** — 网络搜索工具，支持搜索关键词，返回标题、URL 和摘要

### 高级工具

- [ ] **spawn** — 后台子代理创建，支持传入指令和等待结果，用于并行任务执行
- [ ] **mcp** — MCP 服务器客户端，连接外部 MCP 服务并将其工具包装为原生工具
- [ ] **self** — 运行时状态检查，返回当前 session 信息、配置、可用工具列表等
- [x] **message** — 向用户发送消息（用于 channel 推送场景，如 telegram/whatsapp）
- [ ] **notebook** — Jupyter notebook (`.ipynb`) 编辑工具，支持代码单元格和 markdown 单元格增删改
- [ ] **sandbox** — shell 命令沙箱后端，支持不同的执行隔离策略（docker/ssh/local）

### 可选 Skills（nanobot 内置）

- [ ] **github** — 使用 `gh` CLI 与 GitHub 交互（PR/Issue/repo 操作）
- [ ] **weather** — 通过 wttr.in 和 Open-Meteo 获取天气信息
- [ ] **summarize** — 摘要生成技能（URL、文件、YouTube 视频）
- [ ] **tmux** — 远程操控 tmux 会话
- [ ] **clawhub** — ClawHub 技能搜索和安装（公共技能注册表）
- [ ] **skill-creator** — 创建新技能的辅助工具

## 增强 config 模块

- [x] 使用 `define_config!` 宏定义所有可配置项（`AgentConfig`、`ProviderConfig`），自动生成结构体、默认值、clamp 逻辑和 `FieldMeta` 元数据
- [x] 宏支持多种约束：`range(min, max)` 数值范围、`str_max(n)` 字符串截断、`allowed([...])` 白名单、`allowed_max([...], n)` 白名单+长度、`none` 无限制
- [x] 配置全局单例：`Config::init(path)` 初始化，`Config::get()` 返回 `Arc<Config>` 快照
- [x] 文件监听：`notify` crate 监控 config.json 修改，自动重载
- [x] 热重载 diff 通知：修改后计算新旧配置差异，通过 `Config::subscribe(path, callback)` 通知订阅者
- [x] `#[serde(flatten)]` 保留未知 JSON 字段，实现向前兼容
- [x] `Normalizable` trait 提供模块级归一化钩子，在 clamp 后调用
- [x] `setup` 命令通过 `AgentConfig::default()` / `ProviderConfig::default()` 生成完整默认配置
- [ ] 订阅者回调集成到运行时组件（如 gateway 热切换 provider 配置）
- [ ] `Config::get()` 快照支持 per-provider 按需获取配置（避免全量拷贝）
- [ ] 配置写保护：运行时通过 API 锁定关键配置，禁止热重载覆盖

## Heartbeat 机制

- [x] 实现 heartbeat 调度器，固定间隔时间运行任务
- [x] 支持 `config.json` 配置 `gateway.heartbeat.enabled` 和 `interval_s`
- [x] `AgentLoop` 初始化时启动 heartbeat 定时器（gateway 模式）
- [x] 定时器触发时读取 `workspace/HEARTBEAT.md`，通过 `AgentLoop.run_task()` 执行任务
- [x] 执行完成后通过 outbound 投递结果到默认 channel
- [ ] 支持 `workspace/heartbeat.jsonl` 任务格式（多任务调度）

## Cron 定时任务机制

- [x] 实现 `cron` 工具（add/list/remove actions），供 Agent 管理定时任务
- [x] Cron 服务实现：`CronService` 支持 `at`/`every`/`cron` 三种调度
- [x] JSON 持久化存储（`workspace/cron/jobs.json`）
- [x] 后台 cron 调度器，每秒 tick 检查并触发到期任务
- [x] 触发的任务通过 `AgentLoop.run_task()` 执行
- [x] 模型判断执行结果是否需要 deliver 到 channel（`payload.deliver` + `channel`/`to` 字段）
- [x] 所有 cron 消息统一记录到 `system:cron` session，最多保留 2000 条消息（滑动窗口）
- [ ] **阻止 cron 内再调度 cron** - `CronTool` 曾有 `in_cron_context` 字段意图阻止在 cron 触发的 run 内新增 cron job，但从未接通（已于 2026-08-01 tool context 重构中作为死代码删除）。后续可基于 `TurnContext` 增加 `TurnKind`（User/Cron/Heartbeat）字段，在 `add_job` 中判断 `kind == Cron` 时拒绝调度。

## Memory 记忆系统

- [x] 实现 memory 模块（独立模块，非工具），管理 `workspace/memory/MEMORY.md` 和 `history.jsonl`
- [x] 双层记忆：`MEMORY.md` 存储长期记忆（用户偏好、项目上下文、关键决策），`history.jsonl` 存储按时间追加的短期交互记录
- [x] `MemoryStore` 纯文件 I/O 层：读写 MEMORY.md、history.jsonl、SOUL.md、USER.md
- [x] `ContextBuilder` 构建时将 `MEMORY.md` 内容注入 system prompt，标记 `# Memory` 段落
- [x] history.jsonl 格式：每条包含 `cursor`（自增整数）、`timestamp`、`content`
- [x] Dream cursor：`.dream_cursor` 文件记录 Dream 已处理的最后一条 history 游标
- [x] Token-budget 触发 consolidate：LLM 返回 prompt_tokens 后检查预算（`context_window_tokens - max_completion_tokens - SAFETY_BUFFER`），超出时触发摘要
- [x] Consolidate 由模型对当前 session 的对话进行摘要，结果追加到 `history.jsonl`
- [x] Session 记录 `last_consolidated` 游标，已 consolidate 的 message 不再添加到上下文中
- [x] 用最后一次 consolidate 的摘要（`last_summary` 字段）代替所有已 evict 的 message 内容，注入到 system prompt 的 `[Resumed Session]` 段落
- [x] Consolidate 在 user-turn 边界进行消息驱逐（eviction），保证语义完整性
- [x] Consolidate 支持会话摘要持久化到 `SessionMeta`，重启后可恢复
- [x] Consolidate 使用 `char_per_token_ratio` 估算消息 token（`total_chars / prompt_tokens`，默认 4.0）
- [x] Consolidate 失败时降级处理（`archive` 返回错误时静默跳过，不中断主循环）
- [x] Session 持久化重构：meta 数据单独 `.meta.json` 文件，messages 使用 append-only 写入 JSONL，每条消息自增 ID，consolidation 游标基于 ID 而非数组索引
- [ ] Agent 可通过工具调用主动读取/搜索/更新记忆
- [x] 移除当前 `src/tools/memory.rs` 中的三个独立工具实现，改为 memory 模块内部管理

## Dream 定时记忆整理

- [x] 实现 DreamService 模块（`src/dream.rs`），提供 build_prompt/build_tools/build_provider/run 方法
- [x] Dream 通过 cron 定时触发（`gateway.dream.enabled` + `interval_ms` 配置），默认每 2 小时
- [x] 支持 `/dream` 手动命令触发（AgentLoop-tier 命令）
- [x] 支持 `model_override` 配置，允许 dream 使用不同的 provider
- [x] Dream 使用受限工具集（file_reader + 受限 file_writer/file_editor），仅可编辑 SOUL.md、USER.md、memory/MEMORY.md、skills/
- [x] Dream prompt 模板从 `resources/templates/dream.md` 嵌入，包含四文件路由、MECE 分类、历史属性标签、删除/保留启发式
- [x] Dream 完成后推进 dream_cursor，并调用 compact_history_after_dream 清理已处理条目（保留最近 50 条）
- [x] Dream 使用独立 session `system:dream`，不配置 consolidator
- [ ] ~~支持通过 git log 为记忆行添加年龄标注~~ — 票 06 已否决：年龄由 `memory/.snapshots/` 快照序列 diff 反推，不用 git（见 `docs/adr/0014`）
- [ ] ~~Dream 编辑结果记录到 history.jsonl 作为审计日志~~ — 票 06 已定：审计接 trace（session `system:dream`）+ 指纹级（旧/新内容指纹、类别/文档名、`EditOutcome`、处理行范围），全文走票 15 落盘引用；不建独立审计文件

## Context Builder 对齐 nanobot

- [x] **Identity 段落**：将当前 `fixed_intro()` 替换为结构化 identity 段落，包含 runtime 信息（OS、平台、工作区路径），参考 nanobot `identity.md` 模板
- [x] **Platform Policy**：在 identity 中注入平台策略（POSIX/Windows），提示 file tool 优先于 shell 命令
- [x] **Channel Format Hint**：根据当前 channel 类型（telegram/whatsapp/cli/email）注入格式提示，指导模型适配输出格式
- [x] **Runtime Context 块**：在 user message 前注入 `[Runtime Context]` 元数据块（Current Time、Channel、Chat ID），而非拼入 system prompt
- [x] **Session Summary 注入**：AutoCompact/Consolidate 后的摘要以 `[Resumed Session]` 标记注入 runtime context，而非 system prompt
- [x] **Skills 加载区分 always/可选**：区分 always skills（始终加载完整内容）和可选 skills（仅显示名称列表，由 Agent 按需读取）
- [x] **Recent History 注入**：system prompt 末尾追加 `# Recent History` 段落，从 `history.jsonl` 读取未处理的最近 N 条（nanobot 默认 50 条）
- [x] **Memory 跳过模板**：`MEMORY.md` 内容与模板一致时不注入 system prompt（对齐 `_is_template_content` 逻辑）
- [x] **Message 合并**：当 history 最后一条消息与当前消息 role 相同时，合并内容而非追加同 role 消息（避免 provider 拒绝连续同 role 消息）
- [x] **Media 支持**：`build_messages` 支持 base64 图片内联（多模态消息格式）
- [x] **工具结果追加**：`add_tool_result` 辅助方法确保 tool message 格式正确（tool_call_id + name）
- [x] **Thinking/Reasoning 支持**：assistant message 支持 `reasoning_content` / `thinking_blocks` 字段（为未来 reasoning 模型预留）

## AutoCompact 空闲会话压缩

- [ ] 实现 AutoCompact 调度器，TTL-based 空闲会话检测（config 可配置 idle TTL，默认 24h）
- [ ] 定时扫描所有 session，识别超过 TTL 未活动的会话
- [ ] 对空闲会话执行压缩：分离未 consolidate 的尾部消息，保留最近 N 条消息（默认 8 条）
- [ ] 使用 Consolidator 对剩余消息进行 LLM 摘要
- [ ] Session 重新加载时注入摘要到 system prompt，代替已归档的消息
- [ ] 压缩结果持久化到 session JSONL 文件

- [ ] **时间戳注入**：在构造 context 时，注入的 history 消息中，每条 `role=user` 的消息需在开头增加时间戳，用于处理相对时间引用（如"昨天xxx"、"刚才"等）
- [ ] **WebUI 空消息过滤**：WebUI 显示时，过滤掉 trim 后内容为空的消息，避免空白气泡占用界面

## 用户命令处理机制

- [x] 实现三层命令处理架构
- [x] **Channel 层**：`/quit`、`/exit` 在 IoScheduler 中拦截，绕过 MessageBus 直接触发退出
- [x] **AgentLoop 层**：`/stop`（停止所有运行中任务）、`/clear` 或 `/new`（清空当前会话）、`/status`（显示会话状态）
- [x] **AgentRunner 层**：其他斜杠命令（如 `/help`）作为正常任务流经 ReAct 循环，由模型处理
- [x] `/stop` 使用 per-session CancellationToken 机制取消当前和队列中的所有任务
- [x] `/stop` 取消后 enqueue 哨兵 `/stop` User 消息，等待所有 pending task 排空后返回 "all tasks cancelled"
- [x] AgentRunner 在运行开始、LLM 请求前、每个 tool call 前检查取消状态
- [x] 退出命令直接触发：cancel 所有 task、停止所有 channel、shutdown thread pool，然后正常退出

## CLI Markdown 输出支持

- [ ] 增加 CLI 输出的 Markdown 格式处理
- [ ] 解析 LLM 返回的 Markdown 内容并在终端中渲染（加粗、代码块、列表等）
- [ ] 可选使用 `comfy-table` / `termimad` / `bat` 等库实现终端 Markdown 渲染
- [ ] 保持与非 Markdown 通道（如 Telegram）输出兼容性

## LLM Cache 功能

- [x] `ProviderConfig` 新增 `prompt_cache_enabled` 字段（默认 true），通过 `define_config!` 宏定义
- [x] OpenAI provider 在最后一条 system message 的 content 上注入 `cache_control: {"type": "ephemeral"}`
- [x] Runtime context 稳定性：`build_messages` 接受预生成的 `runtime_ctx` 参数，`AgentRunner` 在 turn 开始前生成一次，确保同一次 turn 内多次迭代使用相同的时间戳，提升缓存命中率

## Runtime Context Placement 修复（CRITICAL BUG）

> **状态：** 2026-06-20 发现缓存控制标记（cache_control）_placement_ 逻辑错误，导致 volatile data（runtime context）被缓存而非 stable data（user content）。

- [x] **修复 cache_control placement 逻辑** — 将 `cache_control` 从 runtime context block（最后一个 content block）移动到 user content block（第一个 content block），确保 stable 的用户输入被缓存而非 volatile 的时间戳。

  **修复内容：** 
  - 在 `src/provider/openai.rs` 中，将 runtime_content 的插入方式从 `insert(0, ...)` 改为 `push(...)`，使 runtime context 出现在用户内容之后
  - 将 `cache_control` 标记放置在 `content_blocks[0]`（user content），而不是 `content_blocks[last]`（runtime context）
  - 添加了 `MockProvider` 用于测试，解决了 `test_agent_loop_run_task_nonexistent_session` 测试超时 60 秒的问题
  - 添加了完整的测试覆盖：`test_runtime_content_placed_after_user_content` 和 `test_cache_control_on_runtime_content_block`

## Gateway 模式

- [x] `slimbot gateway` 命令启动 cron、heartbeat 和 enabled channels
- [x] CLI channel 不在 config.channels 中配置，仅在 CLI/agent 模式隐式启用
- [x] Channel 配置 key 即为 type，无需 `type` 字段
- [x] WebUI channel：axum HTTP server，SSE 流式输出，嵌入 index.html
- [x] Gateway 主线程为 ChannelManager 监听 output message
- [x] Cron 与 Heartbeat 集成到 AgentLoop 通过 callback 触发任务
- [x] 完整的单元测试覆盖：cron service (38 tests), heartbeat service (12 tests), webui channel (11 tests), channel manager (9 tests), message tool (6 tests), cron tool (13 tests)

## 代码优化（2026-05-24 审计）

> 注：2026-06-19 本次仅完成第 155 项；第 154 项经核查已由 `SessionManager::graceful_shutdown()` 完成；第 153 项经核查 `AgentRunner` 已作为字段复用于 `AgentLoop`；第 157 项需新依赖（`arc-swap`）或重新设计 reset 语义，单独留待后续。

### 高优先级（性能 / 正确性）

- [x] **复用 AgentRunner 实例** — `AgentRunner` 已作为字段复用（`agent_loop.rs:63` `runner: AgentRunner`）
- [x] **合并 shutdown_session_memory 三次锁为一次** — 已由 `SessionManager::graceful_shutdown()` 合并 `wait_all_idle` + `sync_all_meta` 到单次锁内，且为唯一生产调用路径（2026-06-19 核查确认）
- [x] **移除 MessageBus 中 `Arc<Mutex<Receiver>>` 包装** — 已改为 `new()` 返回 `(Arc<MessageBus>, MessageBusReceivers)`，receiver 按所有权移交唯一消费者，并新增 `publish_inbound` / `publish_outbound` 发送封装（2026-06-19）
- [ ] **缓存 ToolManager 的 `to_openai_functions` 结果** — 工具定义是静态的，但每次 LLM 调用都重新分配 `Vec` 并 clone 所有字符串。改为内部缓存，注册时重建（`tool.rs:120-129`）
- [ ] **移除 CancellationToken 的 Mutex 包装** — 当前 `Mutex` 用于保护 `cancel_and_reset()` 中的 token 交换（`/stop` 重置语义需要），不是冗余锁；真正去锁需引入 `arc-swap` 新依赖或重设 reset 语义（`session.rs:32`）

### 中优先级（代码质量）

- [ ] **消除 AgentRunnerBuilder 和 SessionTaskBuilder 重复** — 两个 builder 携带几乎相同字段和 build 逻辑。提取共享 `AgentComponents` 结构（`runner.rs:46-156`, `session.rs:373-518`）
- [ ] **统一使用 parking_lot::Mutex 替换 std::sync::Mutex** — 项目已依赖 `parking_lot`，但 cron/service.rs、tools/message.rs、session.rs 仍使用 `std::sync::Mutex`
- [ ] **拆分 AgentLoop God Object** — 持有 8 个组件引用，职责过重。引入 `Runtime` / `App` 结构负责生命周期管理（`agent_loop.rs:63-74`）

### 低优先级（改进空间）

- [ ] **升级 reqwest 到 0.12** — 当前使用 0.11，0.12 支持 HTTP/3 和改进的 TLS 处理（`Cargo.toml:18`）
- [ ] **补充测试覆盖** — FileReader 缺少成功读取/截断测试、路径安全缺少 symlink 越界测试、Cron 缺少并发 tick 测试、Session 缺少并发提交竞态测试
- [ ] **优化 Message 序列化避免中间 Value 分配** — `serde_json::json!()` 每次 LLM 调用为每条消息分配中间 JSON Value。实现自定义 `Serialize` 直接写 OpenAI 格式（`provider/openai.rs:118-203`）

## personal agent 触发器（已实现，2026-10-31）

- [x] **Wayfinder map** — 触发器体系（数据源采集与快照、订阅判定、subagent 承载、codemode 运行时、与 cron 的接缝）的决策地图：`.scratch/personal-agent-triggers/map.md`；票文在同目录 `issues/`，终点产物是 spec（票 08）
- [x] **研究票 05（已完成）** — codemode 运行时选型选 rquickjs 0.14（详见 `.scratch/personal-agent-triggers/research-codemode-runtime.md`）；待补：`target/probe/` 的编译体积实测（受网络阻塞）
- [x] **票 01（已完成）** — subagent 机制：`agents/<name>.md` schema、增量工具与拒绝启动、`subagent{agent,task}` 异步契约、内存会话跑完即删、`[Subagent result]` 回流文案
- [x] **票 02（已完成）** — 采集与快照层：`triggers/sources/<id>/`（meta.json + collect.js）、id 系统生成、`{items}` 归一化 + freeze/commit 一致视图、固定 tick 60s、快照不落盘、`trigger_sources` 只给结构不给值
- [x] **票 03（已完成）** — 订阅定义与判定：`triggers/triggers/<id>/`（meta.json + condition.js + state.json）、`judge(ctx)` 返回 `{fire, evidence, detail}`、边沿+cooldown 状态机、ready 门槛、不自动 disable
- [x] **票 04（已完成）** — 触发执行与投递：origin 同 cron payload、`publish_inbound` 回主 Loop、同 tick 合并、失败告警例外、三个并列 root span、`history.jsonl`
- [x] **票 06（已完成）** — 时间源与 cron 接缝：独立 `interval(60s)` tick、`schedule` 复用 `CronSchedule` 但不入 `jobs.json`、只用本地时间、`ctx.now()/boot_at()/uptime_s()`、重启不补触发、只读的 `trigger_test`
- [x] **票 07（已完成）** — 提示词/skill/文档：触发器只以内置 agent `agents/trigger.md` 存在（一个能力一个入口）、两个多 action 工具、GLOSSARY 九词、ADR 0006-0009
- [x] **票 08（已完成）** — spec 汇编：`spec.md` 为权威规格，拆出实现票 10-14；实现细节（路径/行号/片段）存档在票 08 记录里
- [x] **实现票 10（已完成）** — subagent 运行时：`src/agent/`（定义扫描校验、ToolCatalog、per-run 工具装配、并发闸、`subagent` 工具、内存会话跑完即删、`[Subagent result]` 回流）
- [x] **实现票 11（已完成）** — codemode 沙箱：`src/codemode/`（rquickjs 专职线程 + current-thread runtime + LocalSet、interrupt + 外层 deadline 双层超时、宿主函数白名单、Redactor 脱敏）
- [x] **实现票 12（已完成）** — 源采集与快照：`src/trigger/source/`（mtime 热加载、collect 脚本、`{items}` 归一化、freeze/commit 一致视图、健康度与降频）+ `trigger_source` 工具
- [x] **实现票 13（已完成）** — 判定与状态机：`src/trigger/subscription/`（judge 脚本、边沿+cooldown、ready 门槛、store/state.json 宿主独占写、试跑）+ `trigger` 工具
- [x] **实现票 14（已完成）** — 执行投递与历史：`src/trigger/dispatch.rs`（有界队列、deliver 三级求值、同 tick 合并、失败告警例外、history.jsonl 六字段、三个并列 root span）
- [x] **文档收口（票 07 §8 清单）** — 新增 `docs/trigger.md`、`docs/agent.md`、`docs/codemode.md`；同步 `docs/index.md`、`docs/architecture.md`、`docs/context.md`、`docs/tool.md`、`docs/tools.md`、`docs/agent_loop.md`、`CLAUDE.md`、`cron` 工具描述、memory skill 的 agents/skills 边界
