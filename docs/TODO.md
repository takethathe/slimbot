# TODO

## personal agent v1（wayfinder 建图中，2026-10-31）

- [x] **能力调研** — 行业一手资料 + 代码核对的缺口分析：`.scratch/personal-agent-capabilities/research.md`
- [x] **Wayfinder map** — 五件底座（信任边界、记忆写侧、MCP + 一个真实集成、一个 IM 通道、可靠性）的决策地图与 15 张票（票 15 上下文压力由票 05 毕业新增）：`.scratch/personal-agent-capabilities/map.md`；目的地是 `spec.md` + ADR，落稿后交 `/implement`
- [x] **WebUI 认证与网络边界** — 纯本机入口、无认证、只监听 loopback + 四条浏览器侧中间件；已回填票 02，决策见 `docs/adr/0010`
- [x] **工具风险与审批策略** — 5 类风险级 → 3 档管控；审批以回合结束 + 下一轮兑现，无 `ask_user` 工具；已回填票 03，决策见 `docs/adr/0011`
- [x] **记忆写侧语义** — 单文件漏斗（主模型只用 file 工具写 MEMORY.md）+ 分类/晋升归 Dream + Protected document 代码强制；已回填票 05，决策见 `docs/adr/0012`；连带判定 `Daily` v1 不建设、history 不进检索语料
- [x] **上下文压力（Context pressure）** — 单一窗口锚 + 占比派生全部长度限制；清理与可找回配对（引用带落盘路径、必要时补落盘）；豁免改为工具自述 `context_exempt()`；已回填票 15，决策见 `docs/adr/0013`；连带两条 breaking change（废弃 `agent.max_tool_result_chars`、`context_window_tokens` 移到 provider 段）
- [x] **记忆生命周期** — 把已做判断的地方接上容量（不新增 flush、不多新机件）：summary 预算 512 字节→窗口 2% 占比、history 只删已处理连续前缀、Dream 接 trace + 指纹级审计（不建独立审计文件）、改读真实配置（两锚分离）、回滚靠写前快照（`memory/.snapshots/` 分层轮转、不用 git）、条目不写日期、年龄由快照序列 diff 反推、成功判定换结构判定；已回填票 06，决策见 `docs/adr/0014`；回溯修订票 05 provenance 约定与票 15 文档预算锚归属
- [x] **检索与存储（research 票）** — v1 不新增任何检索机件（向量/sqlite/增量/transcript/评估集均出 scope）；主召回是整篇注入不是检索，现有 BM25-lite + 内存派生索引继续；接缝在 `MemoryIndex` trait + `SearchMode`（ADR 0004）；已回填票 07，决策见 `docs/adr/0015`，调研纪要见 `.scratch/personal-agent-capabilities/research-retrieval-storage.md`
- [x] **MCP 客户端范围与安全（research 票）** — MCP 工具不进模型 schema，只通过 codemode 沙箱发现与调用；searchTools 复用 KeywordIndex BM25-lite；Gateway 级 McpService 管理连接；断连退避摘除；stdio + streamable HTTP；配置归 `config.json` 的 `mcp.servers[]`；验收集成 filesystem；已回填票 08，决策见 `docs/adr/0016`
- [x] **首个 IM 通道与身份** — 飞书为 v1 首个 IM 通道（Telegram/邮件降为后续）；Rust 原生 channel 坐社区 crate `larksuite-oapi-sdk-rs`（只开 `channel` feature、锁版本、禁 axum）；企业自建应用 + 单条 WS 长连、gateway 为唯一连接持有者；`channels.feishu.owner_open_id` 绑 owner；仅 p2p、群组关闭；session_id `feishu:{oc_…}` 两段式不变；流式卡片/图片/附件/markdown 全进 v1；隐私护栏（锁版本+audit、永不记正文/open_id/tenant_key/app_secret/token、入站媒体只按 file-key 下载防 SSRF）；已回填票 09，决策见 `docs/adr/0017`
- [x] **出站投递路由** — 投递模型大部分已存在（`BusResult{session_id}`→outbound→`ChannelManager` 前缀路由），本票 codify + 去 heartbeat 硬编码，不新建 DeliveryTarget 网络/持久 outbox/全局投递账本；目标解析优先级 = 显式 deliver-to（创建时指定）> 创建任务的 origin 会话 > owner 主通道（兜底）；唯一静默条件 = `deliver:false`；轻量 `DeliveryTarget{channel, chat_id}` + 共享 `resolve_target`；防打扰沿用 trigger 既有（quiet hours/去重/分级出 v1）；best-effort 无 outbox、重启不补投；审批只约束 agent 主动 `message`；可观测复用 per-source + trace；已回填票 10
- [x] **可靠性与预算** — v1 走最小可靠闭环、其余推迟（事实核查：message_bus 已 bounded、token 已进 trace、OTel trace backend 已存、取消路径现成）；超时 = 给 `run()` 套挂钟 deadline 且复用 `/stop` 取消路径（`timeout_seconds` 从此有真身）、provider 超时入配置；调度时区全库统一 `chrono::Local`、不引 chrono-tz（修 UTC/Local 双把表）；停机只补记 `skipped`、绝不补跑；瞬时错有限退避重试、不做 fallback provider；不设成本硬上限（额度交 provider 提示）；provider 错误按 `code` 分类、配额/鉴权类不重试且把原话经投递路径递 owner、响应体永不落 session/日志（隐私）、重复硬错告警合并；不建 `/health`/OTel metrics（靠 trace+日志，OTel trace 经 config 接 collector）；已回填票 11，决策见 `docs/adr/0018`
- [x] **触发器接缝与排期** — 触发器 effort 已决策且大部分实现落地（a8eeaa0），本票 = 接缝规则非排期：trigger = 已落地基线（不列 M3 gate）；投递“谁触发就投给谁（origin）、除 `record`/`deliver:false`”，票 10 `resolve_target` 的 owner 兜底只对无 origin 源生效、trigger origin 不可达维持 book-only（迁移 `destination()`→`resolve_target` 是唯一真代码接缝）；时间源不统一实现只声明一致（trigger 自带 Local、票 11 改 cron 不触及它）；evidence→记忆继承票 05+ADR 0009、只落消化表述+provenance 标注、原文不入记忆、不建 taint-tracking；审批继承票 03 无人值守=Deny；不新增术语、subagent/采集/判定/codemode 权威仍归触发器票 01–08；**不建 ADR**；已回填票 12
- [x] **v1 差异化范围** — 开题时用户引入**执行器（Executor）+ 连接器（Connector）**（术语 Q5–Q9 收口）：执行器进 v1、为与采集器镜像的一等 codemode 动作原语（仅 codemode 内、subagent 创建与调用、按需执行）；连接器 = codemode 可直接调用的外部能力封装（**MCP server 是其特例**、v1 唯一实例），Channel 与它正交、json-render 卡片属 Channel 展示不算连接器、内置工具/时间网络原语都不是；审批走票 03（`external_send`/无人值守=Deny）；v1 一个真实集成 = GitHub 式 MCP 连接器 + 执行器；完整镜像实现另立子 effort。**差异化边界写死**：D1=已落地触发器+连接器动作；D2=票 05+06无 WebUI diff、回滚走文件快照；D3 卡片 actions 回传/语音/**D4 技能供应链**/**数据导出工具均出 v1**（审批不需按钮、数据即明文文件）；附 5 条成功场景；已回填票 13，决策见 `docs/adr/0019`，GLOSSARY 新增 `### Connector`
- [x] **研究票：密钥与 OAuth 凭证管理** — 结论写入 `.scratch/personal-agent-capabilities/research-secrets-oauth.md`，已回填票 04；keychain 优先 / env 次之 / 0600 配置兼容
- [x] **单用户身份模型** — 显式单例 owner；共享记忆 / 分通道会话；群聊只有 owner 可授权；结论回填票 01
- [x] **触发器设计（wayfinder 已收官，2026-10-31）** — 七张决策票全部 resolved，spec 与 ADR 已落稿：`.scratch/personal-agent-triggers/spec.md` + `docs/adr/0006..0009`；ADR 篇目：subagent 运行时形状、触发器是 cron 之上的一层、codemode 运行时选型、外部内容不进上下文
  - [x] 01 subagent 机制 [x] 02 采集与快照 [x] 03 判定语义
  - [x] 04 执行与投递 [x] 05 codemode 运行时 [x] 06 时间源
  - [x] 07 提示词/skill/文档 [x] 08 spec 落稿
- [x] **spec 落稿（票 14，2026-11-01）** — `.scratch/personal-agent-capabilities/spec.md`（`ready-for-agent`），汇总决策票 01–13 + ADR 0006–0019；wayfinder map 已 closed
- [ ] **v1 实现票（/to-tickets 拆出 13 张，2026-11-01）** — `.scratch/personal-agent-capabilities/issues/16–28`（均 `ready-for-agent`）：16 owner+WebUI 本地边界 / 17 resolve_target 投递接缝 / 18 时区统一+run deadline+skipped / 19 provider 错误分类+超时+重试透传 / 20 上下文压力+两项破坏性 config 迁移 / 21 审批全链路 / 22 密钥解析链 / 23 记忆单文件漏斗 / 24 记忆生命周期快照回滚 / 25 codemode agent 入口（tools.*+searchTools）/ 26 MCP 连接器+GitHub 首集成 / 27 飞书 p2p 文字 / 28 飞书流式卡片+图文。当前 Frontier（无阻塞）：**16、18、20、21、22、23**。执行器完整镜像另立子 effort map（镜像 triggers），不在本组票内
- [x] **票 20 落地（上下文压力 + 两项破坏性配置迁移）** — `src/pressure.rs` 定义唯一锚（`providers.context_window_tokens`，默认 32768）与占比表（单条 25% / 预览 4% / 余量 3% / 触发 50% / 目标 60% / 摘要 2% + 绝对夹取），冷启动比例改 3.0；`src/config_migrate.rs` 一次性迁移旧 `agent.context_window_tokens`（搬到对应 provider 并写 `[config migration]` 日志）与废弃 `agent.max_tool_result_chars`；`Tool::context_exempt()` 自述取代中央名单（`file_reader`/`memory_search`/`message`）；`snip` 两阶段清理的引用均带 `tool-results/<id>.txt` 路径，未落盘者清理时补写；`tool-results/` 按 `pressure.tool_results_ttl_days` 由 Dream 成功收尾清扫（无新定时器）；`setup` 与 WebUI `config_schema` 同步
- [x] **票 22 落地（密钥解析链）** — 新增 `src/secrets.rs`：`SecretStore` 按 **OS keychain → 环境变量 → 配置文件** 严格解析（先命中先赢，keychain 不可用静默降级），作用于 provider `api_key`（键 `slimbot.<provider-name>`）与 `triggers.secrets`（键 `slimbot.trigger.<secret-name>`）；值以 `Secret` 包装（`Debug`/`Display` 恒为 `***`），解析路径只记 key + 来源，`tests/secrets_integration.rs` 抓日志断言无值泄露；含密钥的 `config.json` 写出/加载即 `0600`（`#[cfg(unix)]`，不含密钥的文件不动权限）；缺失时仍报原 `api_key must not be empty` 家族错误并列出三条路径；顺手修掉 WebUI schema 把 `triggers.secrets` 整段明文塞进 `triggers` 段的问题（现在只声明名字）；WebUI 保存的校验改在“解析后副本”上跑（keychain 里的 key 不会被误报缺失，写回的仍是文件形态）；文档见 `docs/config.md`「密钥与凭证」
- [x] **票 21 落地（审批全链路）** — 新增 `src/approval/`（`RiskLevel`/`ControlLevel`/`PendingApproval`/`ApprovalStore`/`merge_approval_rounds`/`protect_start`）与 `runner/hooks/approval.rs` 四个 hook（PreTurn 开局结算 TTL、PreTool 唯一决策点、PreChat 构建时折叠、AfterTurn 收尾清理）；`Tool::risk_level()` 不可配且无默认实现，全部内置工具已声明；配置 `tools.approval.{levels,tools,timeout_seconds}`（`tools` 段改为 `{entries, approval}`，旧裸数组仍可加载，不热重载）；`Deny` 续跑、`Ask` 结束回合 + 纯文本清单、下一轮按 `(tool, args_hash)` 兑现、不匹配否决并删记录、TTL/重启作废、无人值守（`TurnKind` ≠ `User`）降级为 `Deny`；未兑现的引导结果改写为失败文案而非删除（每个 `tool_call` 恒有真结果）；待兑现回合被 snipping/consolidation pin 住；审计落 trace `approval` span 且不进 session JSONL；兑现前广播 `ApprovalRedeemed` 供 `/stop`
- [x] **票 23 落地（记忆单文件漏斗）** — 主模型仍只用 `file_writer`/`file_editor` 写 `memory/MEMORY.md`，**零新增工具**（`test_tool_factory_registers_no_memory_write_tool` + `test_agent_visible_tools_are_read_only_for_memory`）；新增 `src/tools/protected.rs`：保护集从 `bootstrap::protected_documents()` 派生（AGENTS/USER/SOUL/TOOLS + `agents/trigger.md`，排除 MEMORY.md）再加 `skills/*/SKILL.md` 与 `memory/*.md`，`file_writer`/`file_editor` 命中返回 `REFUSED:` 工具结果（不退回合、不 panic），判定在路径解析之后（`sub/../SOUL.md`、symlink 不可绕过，大小写不敏感）；`MEMORY.md` 模板与 memory skill 改写（不再写 "Do NOT edit directly"），provenance 定为写法约定 `(source · YYYY-MM-DD)`、代码不解析；外部原文只到引用路径、`MEMORY.md` 只收消化表述（`test_memory_write_funnel_keeps_verbatim_external_blob_out_of_memory`）；检索语料不含 `Daily` / history（`test_search_corpus_excludes_daily_documents_and_history`）；文档见 `docs/tool.md`「受保护文档」与 `docs/memory.md`「写侧：单文件漏斗」
- [x] **票 25 落地（codemode agent 入口）** — 第三入口 `ENTRY_AGENT`（脚本定义 `async function main(ctx)`，`Limits::agent()` 30s/64MB/64KB）：`src/codemode/agent.rs` 的 `AgentHost` 只装 `ctx.tools.<name>(args?)`（来自本运行 `ToolManager` 装配，未装配的工具结构性缺失）与 `ctx.searchTools(query, limit?)`（复用票 15 `KeywordIndex` 的 tokenize 与 BM25-lite 打分核心，名字命中 ×2 权重，排序确定性），其余仍是时间三件 + `log`，无 fs/http/secret/store；模型侧新增 `src/tools/codemode.rs`（默认内置集第 10 个），轮外执行显式报错；嵌套调用走 `src/runner/gate.rs` 的 `decide()`——审批闸决策核心从 `ApprovalHook` 抽出单一实现（hook 只保留 RunState 副作用，17 个既有审批测试原样通过即回归）；**被拒的嵌套调用不中断脚本**：gate 答案文本（`DENIED:` / `NOT EXECUTED:` pending 引导）作为 resolved 值返回、无中途弹窗，attended 轮 Ask 写 pending 并在工具结果尾附 owner 可见清单、下一轮重放同 `(tool, args_hash)` 按兑现放行，无人值守轮 Ask 直接按 Deny；能力清单由测试钉死（agent 面探针 `Object.keys(ctx)` 恰六件 + collect 入口无 `tools`/`searchTools`），全库无脚本文本扫描；**评审后修掉三处嵌套面才暴露的缺陷**：（a）`ApprovalStore::matching` 未过滤 carried → 本轮提出的申请能被本轮重放兑现（脚本连调两次同 args = 自己批准自己），现收紧为只认上一轮遗留（ADR 0011 已明写）；（b）沙箱线程上拿不到 task-local → `message`/`cron`/`trigger` 看到空 origin，`AgentHost` 现在携带 `TurnContext` 并在每次嵌套调用外重建 `with_turn_context` 作用域；（c）不可序列化的参数对象静默变 `Null` 去执行工具，现在返回错误文本且不执行；嵌套调用无自己的 `execute_tool` span（已文档化）；文档见 `docs/codemode.md`「agent 入口（票 25）」与 `docs/runner.md`「审批生命周期」
- [x] **票 26 落地（MCP 连接器 + GitHub 式首集成）** — 新增 `src/mcp/` 三层：`rpc.rs` 自研最小客户端（只说 `initialize` / `tools/list`（游标分页，上限 32 页）/ `tools/call`；结果包 `<codemode_output server= tool=>`，`isError` 加一行前缀仍保留 wrapper；非文本块只留 `[<type> content: <mime>]` 占位）；`transport.rs` 两种传输（stdio 子进程：newline-delimited JSON-RPC + ambient reader task、`env_clear()` 后只放回 `PATH`/`HOME` 与配置点名的变量、stderr **只计字节数**、`kill_on_drop`；streamable-HTTP：一请求一 POST，应答为 JSON 或一帧 SSE，沿用 `mcp-session-id` 与协商版本，错误只报 `safe_endpoint()`（scheme+host+port+path，query 剥掉）且永不回显 body）；`service.rs` 的 `McpService`：每 server 一个 supervisor task（`start()` 立即返回 ⇒ 死 server 不拖住启动、离线即清空该 server 工具缓存 ⇒ 从 `searchTools()` 可搜面摘除、调用 fail-fast 回一条干净错误、`min(2x, backoff_max)` 退避、在线期用带预算的 `tools/list` 活体探测、`shutdown()` notify+并发且有预算的 close+abort）。配置 `mcp.servers[]`（`name`/`transport`/`enabled`/stdio 的 `command`+`args`+`env`/http 的 `url`+`headers`）+ 五个预算字段；`env`/`headers` 里的 `${secret:<key>}` 按 `slimbot.mcp.<server>.<name>` 走票 22 解析链，解不到则**原样送去**（握手失败理由带着它）。**“MCP 不进模型 schema”是结构而不是约定**：`RunToolExec::tool_definitions()`（发模型）只含 `ToolManager`，`callable_definitions()`（脚本可搜可调）才是 ToolManager + 在线连接器；`GateContext::risk_of`/`dispatch` 先查注册工具再查连接器，注册工具永不被遮蔽。风险级 fail-closed：`readOnlyHint` 缺省 `false`、`destructiveHint` 缺省 **`true`**，但 annotation 完全不参与定级 —— `risk_for()` 对任何连接器工具一律返回 `ExternalSend`（把 server 自述接进风险级等于让它给自己提权，`network_egress` 整档默认 allow），hint 只作为「声称」进可调用目录的 description 文本，免审批只能逐工具 override。system prompt 的 Connectors 段（`context.rs::connectors_section`）只报 server 名/状态/工具数，不含任何 server 自述文本。装配归 `AgentLoop`（从 `config.mcp` 建池并 start，无 enabled 条目则不建池；CLI / gateway / 触发器 fire 三条入口同源，`graceful_shutdown` 与 `shutdown_for_cli` 关池），gateway 只记一行开机摘要；状态迁移写日志（上线 `info!` 带工具数、离线 `warn_log!` 带经 `offline_reason()` 收敛的 reason（JSON-RPC 错误只留 code，压成一行并截断）与重试秒数）。测试：`cargo test --lib mcp::`（协议、假 peer 两种 transport、池与退避/重连、命名去重、风险映射）+ `tests/mcp_integration.rs`（GitHub 式 HTTP 读链路、测试二进制自我重入当 stdio server、把 token 打到 stderr 的子进程、回显 Authorization 的 401、日志文件不含 token/`?token=`）+ `tests/gateway_integration.rs`（有/无 `mcp.servers` 时池的有无）；文档见 `docs/mcp.md`、`docs/config.md`「`mcp` — MCP 连接器」，选型见 `docs/adr/0020`（自研最小客户端）。执行器的写侧镜像不在本票（ADR 0019，另立子 effort）
- [ ] **子代理不继承连接器池（票 26 有意未做）** — `SubagentRuntime` 自建 runner 没挂 `connectors`，子代理脚本没有 `mcp__*` 面；接法是在 runtime 构造时把池传下去（它现在靠 11 个位置参数构造，宜先抽 builder）。v1 保持“子代理只用手上的内置工具”
- [ ] **`mcp` 段进 WebUI 配置编辑器** — `config_schema.rs::serialize_schema` 的 section 列表里没有 `mcp` / `mcp.servers`（字段元数据已由 `define_config!` 生成，`env`/`headers` 已标 `secret`），缺的是集合 section 的渲染（`Vec<McpServerConfig>` 增删 + 名字-only 掩码，参照 `providers_section`）；当前只能改文件配 MCP，但 path-patch 引擎不会丢掉已有的 `mcp` 段
- [ ] **`config_schema.rs` 的 `BUILTIN_TOOLS` 缺 `codemode`**（票 25 遗留）— `tools::create_tool` 能造 `codemode`，但编辑器工具名单里没有它，`tools.entries` 无法开关它；`BUILTIN_TOOLS` 注释声称“mirrors `tools::create_tool`”，需对齐
- [ ] **Frontier（下一个 map）** — heartbeat 与触发器的长期边界、群聊 origin 归属已随 capabilities map 移出范围；**执行器/连接器子 effort 已开图（2026-11-01）**：`.scratch/executor-connector/map.md`（7 票，研究票 07 已 resolved，frontier：01/02/03）

## 执行器与连接器子 effort（wayfinder：`.scratch/executor-connector/`）

目的地 = `spec.md`（执行器体系 + MCP 连接器消费面的实现规格）；只产决策不写实现码，开工门槛 = 父票 25/21 落地。已锁前提（ADR 0019 + destination 轮）：执行器=采集器镜像的 codemode 动作脚本、按需执行、动域只对外；连接器 v1=MCP 特例；能力表是唯一安全闸；evidence/审批地基继承 ADR 0009/0011。

## 飞书 IM 通道（票 09 裁决的实现切片，待 spec 落稿后展开）

决策见票 09 + `docs/adr/0017`。前置：`Cargo.toml` 加 `larksuite-oapi-sdk-rs = { version = "=0.3.x", default-features = false, features = ["channel"] }`，标记为安全敏感依赖。

- [x] **配置与注册** — `channels.feishu` 段（`app_id`、`app_secret`(secret 标记 + env override)、`owner_open_id`、`domain`、`streaming`）；`FeishuChannelFactory`（`channel_type = "feishu"`）注册进 `ChannelManager`；gateway 装配时确保唯一连接持有者（票 27 落地：必填校验 + `enabled` 门控 + `resolve_channel_secrets` 命名空间 `slimbot.channel.<channel>.<field>`；v1 实字段为 `base_url`/`verification_token`/`encrypt_key`，`streaming` 随票 28）
- [x] **入站链路** — WS 长连 + crate `channel` 归一化 → `NormalizedMessage` → `publish_inbound`；owner 校验（非 `owner_open_id` 私聊静默忽略、群组丢弃）；session_id `feishu:{oc_…}`（票 27 落地：`classify_inbound` + `InboundRouter` → `BusRequest::owner`；网络隔离在 `FeishuTransport`/`FeishuSender` 接缝后，见 `docs/feishu.md`）
- [ ] **出站流式卡片** — `AgentEvent` delta → CardKit 流式卡片原位刷新 → 收尾定稿；agent markdown → 飞书卡片 markdown 子集映射
- [ ] **媒体** — 入站图片/文件按飞书 file-key 经 im API 下载进 workspace（**不拉任意 URL，防 SSRF**）；出站发图/发文件
- [x] **隐私护栏落地（适配器侧）** — 适配器不记正文/open_id/tenant_key/app_secret/token，错误只落变体+失败类别；marker 守护测试 `test_round_trip_logs_never_carry_pii`（票 27 落地）
- [ ] **crate tracing 逐 call-site 核验** — 确认 `larksuite-oapi-sdk-rs` 的 `tracing` 未泄 PII（否则加 filtering layer）；升级前重 diff 其 tracing/error 面
- [ ] **cargo audit** — 纳入提交前检查；升级前重 diff 其 tracing/error 面
- [ ] **验收** — owner 私聊流式全链路 + 图片/文件收发 + 重启续会话 + 非 owner 忽略 + 日志/session 无明文密钥与正文

## 出站投递路由（票 10 裁决的实现切片，待 spec 落稿后展开）

决策见票 10。大部分投递链路已存在，本切片 = codify 约定 + 去硬编码 + 提炼共享件；不新建 DeliveryTarget 网络 / 持久 outbox / 全局投递账本。

- [x] **提炼 `resolve_target` 公共件** — 把 trigger `dispatch.rs::destination()` 上升为 `resolve_target(explicit, origin, owner_primary) -> Option<DeliveryTarget>`，优先级 **显式 deliver-to > origin 会话 > owner 主通道**；`DeliveryTarget{channel, chat_id}` 两段式（不加 thread/topic/priority）
- [x] **三源共用** — cron / trigger / heartbeat 的投递都改走 `resolve_target`；trigger `destination()` 迁移到它（行为逐字不变，由既有测试锁定）
- [x] **owner 主通道来源** — 运行时记录 owner 首次交互的 `{channel}:{chat_id}`（飞书 owner p2p）作为最后兜底；未建立时降级 log-only
- [x] **heartbeat 去硬编码** — 删除 `gateway.rs` 写死的 `webui:webui_main`；无创建对话 → 落 owner 主通道；可 `gateway.heartbeat.channel`/`to` 覆盖
- [x] **静默语义** — `deliver:false` 是唯一策略静默条件；与“目标不存在”的 log-only 兜底区分
- [ ] **验收** — 在 webui 建的“发到飞书”job → 落飞书（显式优先）；无 target 的系统事件 → 落 owner 主通道；`deliver:false` → 静默；重启不重复投；日志/session 无投递正文泄露（对齐票 09 隐私）

## 可靠性与预算（票 11 裁决的实现切片，待 spec 落稿后展开）

决策见票 11 + `docs/adr/0018`。姿态 = 复用既有机件补齐可预期性，不多造（无预算硬上限/无 /health/无 metrics/无补跑）。

- [x] **run 级挂钟 deadline** — 给 `AgentRunner::run()` 套 `tokio::time::timeout(agent.timeout_seconds)`，超时**走现成 CancellationToken 取消路径**（AfterTurn 回滚/consolidate 照常收尾）；`timeout_seconds` 从此被真正执行（票 18 落地：只包 PreTurn+循环，AfterTurn 在 timeout 之外收尾；run 级 token = 派生的 child token，不误伤同会话排队任务）
- [x] **provider 超时入配置** — `openai.rs` 写死的 reqwest `.timeout(180s)` 改为 `ProviderConfig` 派生字段（票 19 落地：`providers.<name>.timeout_seconds`，默认 180、range 1–3600，非流式与流式同一客户端）
- [x] **调度时区统一 Local** — `cron/service.rs::compute_next_cron` 从 `chrono::Utc` 改 `chrono::Local`，与 trigger 层 + `at` 解析对齐；不引 chrono-tz；`tz` 字段保留不参与计算（标注不生效）（票 18 落地：两路同锚点同值由 `test_cron_expression_resolves_same_wall_time_across_paths` 锁定，`tz` inert 另有测试，见 `docs/cron.md`）
- [x] **错过窗口记 skipped** — 过期 `At` 从“返回 None 静默禁用”改为显式写 `skipped` 状态进 run_history（不补跑）；`Every`/`Cron` 重算下一次保持现状（票 18 落地：宽限 `ONE_SHOT_GRACE_MS = 5min`，`add_job`/`start`/`tick` 三个入口同一判定，作业停用但保留为审计痕迹）
- [x] **provider 瞬时错退避重试** — 只对 429（带 `Retry-After`）/5xx/连接超时有限次（2–3）指数退避重试；无 fallback provider；工具不框架重试（现状保持）（票 19 落地：`send_with_retry` 最多 3 次、`100ms << n` 退避；`Retry-After` 头 v1 不专门读取，符合票面“不特殊处理”裁决）
- [x] **provider 错误分类与投递** — 把 `bail!("API request failed: {status}-{body}")` 拆成按 `error.code`/`type` 分类；鉴权/计费/配额类不重试、本轮失败、**把 provider 原话提炼成通知经票 10 投递给 owner**；`context_length_exceeded` 交票 15；**只提结构化 code/message、响应体不落 session/日志**；重复硬错复用 trigger 告警合并（票 19 落地：`src/provider/error.rs` 五类分类 + `src/owner_alert.rs` owner 告警去重窗口 10min；`Consolidator::force_consolidate` 接 `context_length`；session run 后闭包接线；测试锁定“原话递 owner + 重复合并一条 + body 不落 JSONL/投递”）
- [ ] **验收** — 超 deadline 会话不半截；同一 cron 表达式两路解析同值；过期 At 记 skipped 可见；401/配额不重试且原话递 owner、日志无响应体明文；OTel trace 经 config 指向 collector 可查；无 /health/metrics

## 触发器接缝（票 12 裁决的实现切片，待 spec 落稿后展开）

决策见票 12。触发器已落地，本切片 = 与 v1 底座的集成，非新建。

- [x] **迁移 `destination()`→`resolve_target`** — `trigger/dispatch.rs` 的两级投递（显式→origin→book-only）接进票 10 共享 `resolve_target`；**行为逐字不变**（既有 trigger dispatch 测试锁定）：owner 兜底只对无 origin 源生效，trigger origin 不可达时仍 book-only
- [ ] **spec/architecture 交叉引用** — 在 `docs/architecture.md`/spec 补一段“投递（票10）/可靠性（票11）同样 govern trigger 的出站与失败处理”；无新配置/工具/术语
- [ ] **验收** — trigger 三源（notify/agent/record）投递语义迁移后不变；主 cron UTC→Local 改动不影响 trigger 到期判定（自带 Local）；无人值守 subagent 中/高风险工具 Deny、不阻塞

## v1 差异化与执行器/连接器（票 13 裁决，待 spec 落稿后展开）

决策见票 13 + `docs/adr/0019`（已重写为《执行器…》）。执行器/连接器进 v1 但只定接缝；完整“镜像触发器”实现另立子 effort（新地图，复制触发器路径）。

- [ ] **执行器/连接器写进 spec（仅定位）** — v1 spec 含一节：执行器（与采集器镜像的一等 codemode 动作原语、按需执行、subagent 创建与调用）+ 连接器（codemode 可直接调用的外部能力封装、MCP 为特例）；审批入票 03、外部内容入票 05+ADR 0009；**不展开实现细节**
- [ ] **开连接器/执行器子 effort** — 待 v1 spec 成形后新建 `.scratch/connector/` 地图，镜像 `personal-agent-triggers`：`executors/<id>/{meta.json, act.js}`、actuation HostBindings、连接器（含非-MCP）能力封装、subagent 创建、codemode 调用面、审批粒度（整脚本 vs 逐动作）
- [ ] **v1 边界写死进 spec** — 明写 D3 卡片 actions 回传/语音/D4 技能供应链/数据导出工具均出 v1，各带一句理由；5 条 v1 成功场景作验收基线
- [ ] **验收** — spec 中执行器/连接器定位与 ADR 0019/GLOSSARY `### Connector`/`### Executor` 一致；D1–D4 无残留“待决”；5 场景可逐条映射到已有能力

## 记忆生命周期实现切片（票 06 裁决，2026-10-31）

决策见票 06 + `docs/adr/0014`；票 24 已落地写前快照、分层轮转、Dream trace 审计、结构判定、年龄反推与标记清洗。本票发现的实现级缺陷（先于新功能修）：

- [x] **两个 panic 修复** — `dream.rs` `build_prompt` 的 `&e.content[..500]` 字节切片改为 `truncate_chars`（`char_indices().nth` 落到 char 边界）；`consolidate.rs` summary 保头截断走 `is_char_boundary` 循环（已完成）
- [x] **cursor 归零重复整理** — `compact_history_after_dream` 删除范围限定为“已处理的连续前缀”（`min(cursor, len-keep)`），cursor 删后仍指向第一条未处理，不再 `saturating_sub` 归零
- [~] **summary 预算改占比** — `MAX_SUMMARY_BYTES = 512` 已由票 20 改为 `pressure.summary_ratio`（窗口 2%，256–8192 字符夹取）；剩余：`[skip]` 行进 `history.jsonl` 前在代码里过滤掉
- [x] **Dream 接 trace + 改读真实配置** — `DreamService` 现接收 `agent_config`/`tracer`/`doc_budget`（`agent_loop` 于 `build_tracer` 之后构造并注入），删掉 `AgentConfig::default()` 轻径；两锚分离：截断类取自身窗口、文档预算永远取主 agent provider 窗口（`MemoryBudgetAnchor::primary_from_config`）
- [x] **成功判定换结构判定** — 英文子串改为 `audit.success(result.success)`：run 正常结束**且**本轮无失败写入（零写入合法）；cursor 推进与快照轮转同处成功分支
- [x] **写前快照与分层轮转** — `facade`（Dream 路径）与 `file_writer`/`file_editor`（主模型路径）改写前经 `capture_before_write` 复制旧内容到 `memory/.snapshots/{run}/{文档}`；`SnapshotPolicy` 分层轮转（最近全留→每日→每月 + 硬上限）在 Dream 成功分支 `rotate_snapshots` 触发
- [x] **年龄反推与标记清洗** — `strip_age_markers` 读写两侧剔 `← Nd`/`[Nd]`/`(N days old)`（`memory_edit`/`file_editor` 的 `old_string` **匹配前**就剔）；`entry_ages` 逐快照 diff 反推正文行年龄，只附进 Dream 提示侧信道（标题行不计、缺档标未知、绝不写回文档）

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
- [ ] ~~**切片 3：embedding / hybrid**~~ — **票 07 已裁决出 v1**（向量/sqlite/增量/transcript/评估集均不建）；接缝留在 `MemoryIndex` trait + `SearchMode`（ADR 0004）；重开条件：skill 数量增长到 keyword 漏检明显 / MEMORY.md 预算失效使检索升为主路径（见 `docs/adr/0015`）
- [ ] **切片 4：写侧** — 已由 personal agent v1 票 05 定案并改写：~~`store` / `forget` / `supersede` / dedup gate / 冲突取代~~ 均不新增工具，主模型用现有 `file_writer`/`file_editor` 写 `memory/MEMORY.md`，分类与晋升归 Dream（见 `docs/adr/0012`）；~~把 `hash` 换成稳定哈希（如 sha2）~~ v1 不做（条目身份是标题/正文，跳进程稳定 id 无消费者）。待做：写盘改 `write_file_atomic`、指纹乐观并发校验、`gateway.dream.doc_budget.*` 预算与超限置标记（`retention` / 时间衰减仍待票 06、票 15）
  - [x] **Protected document 拒写（票 23）** — `src/tools/protected.rs`：保护集从 `bootstrap::protected_documents()` 派生（AGENTS/USER/SOUL/TOOLS + `agents/trigger.md`），加 `skills/*/SKILL.md`、`memory/*.md`（MEMORY.md 除外）；`file_writer` / `file_editor` 命中返回 `REFUSED:` 工具结果（不退回合）；零新增 memory 写工具、provenance 写法约定、原文不入记忆、检索语料不含 `Daily` / history 均有测试锁定（`docs/tool.md`、`docs/memory.md`）

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
- [x] **mcp** — MCP 客户端通过 codemode 沙箱暴露（票 08 裁决，ADR 0016）：McpService gateway 级管理 stdio/HTTP 连接、searchTools BM25 发现、断连退避摘除、配置 `mcp.servers[]`；前置基建：codemode-as-model-tool（`src/tools/codemode.rs` + `ENTRY_AGENT`）——**票 26 已落地**，见上「票 26 落地」与 `docs/mcp.md`；filesystem 验收换成 GitHub 式读链路（票 26 验收项）
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
