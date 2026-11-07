# 执行器 = 完全寄生的 ephemeral 无人值守子代理

执行器的全部语义压在一个形状决定上：它既不是主循环里一个可调用的工具，也不是 dispatch 引擎能直跑的一条动作，而是**一个专用子代理（`agents/executor.md`）整体**——创建、更新、执行都发生在这次一次性运行内部，`executor` 工具只是该子代理的增量工具，`act.js` 由子代理在工具调用里同步跑完。主循环对执行器的了解只有 `# Available Agents` 里的一行 `description`。ADR 0019 定的是「执行器是什么」（与采集器镜像的 codemode 按需动作原语）；本篇定的是「它以什么形状存在、以什么姿态运行」（票 20 收口，spec E/H）。

分成这样一个形状，是因为执行器只有一条执行路径：起子代理、由模型驱动、`executor run` 同步返回。任何一条旁路（主 loop 加一个 `executor_run`、trigger 加第四种 `executor{id,args}` kind）都会分岔出第二套投递、第二套审批语义、第二份「怎么用连接器」的知识，而它们彼此迟早漂移。寄生形状让执行器免费继承三件已经在跑的东西：`SubagentRuntime` 的并发闸与 ephemeral 会话、`[Subagent result]` 的投递与 `deliver` 三级求值、以及宿主侧的能力表机制。

两条性质是这条根决策的**推论**，不是并列的第二、第三个决定：

1. **不进主循环**——执行器不存在于主循环的 tool schema 与 system prompt 里，`executor` 工具只注册进 `ToolCatalog`（与 `trigger` / `trigger_source` 同构），只有声明 `tools: executor` 的 agent 装配得到它。因为主循环词汇表与 prompt cache 不该为「一次对外动作」付常驻成本。
2. **v1 Allow-only**——执行器跑在 ephemeral 无人值守子代理里（ADR 0011 明列 subagent 为无人值守来源），因此命中需确认（`Ask`）的连接器写调用**当场按拒绝处理、终止整段脚本、不写 Pending、不回合末兑现**。「回合末兑现」要求「下一轮」，而这个执行体的会话跑完即删，根本没有下一轮；写一条 Pending 只会留下永久死记录。需要 owner 交互确认的动作不是执行器场景，走主循环的工具路径（那里有真下一轮）。

拆成两篇 ADR 会让两边互相复述这条根因；折进 0019 会把那篇从「执行器是什么」膨胀成两个主题。合为一篇，读者读完就知道**为什么这两个反直觉的性质是同一个决定的后果**。它满足 ADR 的三个条件：**难逆转**（工具注册点、`# Available Agents` 可见性、审批姿态都被测试双向钉住，改成主 loop 工具要重写能力表与审批接线）；**无上下文会惊讶**（「为什么不给执行器一个正常工具」「为什么 Ask 直接失败而不是排队等主人」是两个一定会被问到的问题）；**真权衡**（放弃的是「执行器能被主循环顺手调用」与「需要确认的动作也能执行器化」，换来的是零污染、单一执行路径与不会重放整段脚本的幂等安全）。

**Considered Options**

- **完全寄生的 ephemeral 子代理（选定）**：执行器只是一份子代理定义 + 一个 catalog-only 工具。代价是每次执行多一层模型驱动（脚本不能由引擎直跑），以及 Ask 级动作彻底不可用。
- **主循环一等工具 `executor_run`**：调用最直接，但把所有执行体的描述摊进主循环 system prompt，稀释主 loop 注意力并让 prompt cache 随执行器增多失效；也绕过了「动作由模型驱动」这条单一执行路径。
- **trigger 加第四种 `executor{id,args}` ActionKind**：省掉一次子代理，但 dispatch 引擎得自己填结构化 args（现有占位符只渲文本），并且执行器的调用与投递就和普通 `agent` 规则分家了——两条路迟早不一致。
- **整脚本预授权（弹一次审批覆盖一段脚本）**：看似能让 Ask 级动作也跑起来，但风险级是逐连接器工具调用求值的、无法静态算完整脚本，预授权等于用一个无法兑现的承诺换一次放行。**已否**（ADR 0011 的语义是逐调用兑现）。

**Consequences**

- **推论一落地**：`executor` 只进 `ToolCatalog`，工厂在 `AgentLoop::from_parts`（CLI 与 gateway 同源）；回归锁是 `agent_loop.rs` 两个入口各断言「主循环 schema 无 `executor`、子代理 catalog 有它」，外加 `default_tool_set()` 不含它。
- **推论二落地**：`Ask` 与 `Deny` 都是「拒绝 + 终止整段脚本」，`run` 报 in-band `{ok:false, failure:"approval_refused"}`；审批库里**没有** Pending（测试断言 store 为空，不只断言文本），`args_hash` 对执行器也不适用——无兑现即无哈希比对。agent 入口「拒绝文本进脚本、脚本继续跑」的行为刻意保留——它**有**下一轮，分裂只发生在 act 入口，共享的 `decide()` 一个字没改。
- **幂等灾难不成立**：研究纪要最怕的「重放整脚本 → 已成功副作用重跑」要求先有 pending + redeem，执行器压根不进那条路径。换来的是**半执行**：中止前若有 Allow 级写已成功，那就是真的写成了。缓解是编写规约（不把需审批动作当执行器场景、Allow 写用幂等语义、search-then-create），不是宿主级幂等机件（v1 明确不做，spec Out of Scope）。
- **失败一律 in-band**，按根因分五类（审批拦下 / 外部失败 / 宿主闸终止 / 脚本错误 / 未启动），不走 dispatch 的连续失败告警——那是「同一规则退网」的调度语义，与一次按需动作无关。
- **刻意偏离（spec D/J 的字面口径）**：spec 说的「按固有风险级 = 只读筛连接器工具」在落地代码里做不到——`McpService::risk_for()` 对任何连接器工具一律 `ExternalSend`（父票 26 的刻意 fail-closed，有测试 pin）。因此读/写之分改用 **server 的 `readOnlyHint` 声称作能力面过滤**（`readonly_definitions()` / `is_read_only_claimed()`，缺省 fail-closed），**策略面一个字没改**，`risk_for()` 未动。依据与三条理由见 `.scratch/executor-connector/notes/decision-connector-readonly-signal.md`，实现说明在 [codemode.md](../codemode.md#采集面的只读性票-17)。
- **残余风险（如实记下）**：一个撒谎的 server（把写工具标成 `readOnlyHint`）能让写工具进入采集面的只读视图被无人值守调用，也能在 act 侧被当成读侧、绕过 `allow_connectors` 天花板。**绕不过的是审批闸——那条调用在 agent / act 入口仍是 `ExternalSend`**，无人值守即 Deny/中止，除非 owner 已在 `tools.approval.tools` 里逐个放行该 callable。**采集侧没有审批闸**（`SourceLayer` 不持有 `ApprovalStore`），采集面能兜的只有「无 fs 写、无 shell、动域只在 owner 配过的 server 上」——撒谎的 server 在采集器里确实是无人值守、无审批、无人拦。天花板挡的是误配与误写的越权写；agent / act 入口的恶意 server 由审批闸兜底（ADR 0016、0020）。
- **推翻判据**：若将来需要执行器在跨轮会话里连做多步并兑现审批（而不是一次动作），或需要 dispatch 引擎不经模型直跑脚本，那么这条根决策就要重写——那时该同时改 `executor` 的注册点、`agents/executor.md` 的定位与 act 的 Ask 语义，不接受只改其中一处。