# 上下文压力的所有限制由单一窗口锚点派生

slimbot 里控制"上下文能装多少"的机件有六处（AfterTool 落盘与截断、PreChat 的残余清理与长度截断与 snip、run 后的 consolidate），原本各拿一把尺：两个硬编码字符数（16000 / 1200）、一个可配字符数（8000）、三个硬编码条数（10 / 4 / 60）、两个硬编码比例（0.5 / 0.6）。我们决定把**窗口的 token 数作为唯一绝对值**，其余全部表达为**占它的百分比**（按会话的 `char_per_token_ratio` 换算成字符），并给每个百分比配一个绝对下限或封顶。这样"换一个模型"只需要改一个数，而不是让人逐个重算八处长度。

锚点本身从 `agent` 段移到 `provider` 段：窗口是模型属性，不是 agent 策略属性；它成为唯一锚之后，锚放错地方的后果从"snip 裁多一点"升级为"记忆文档、落盘、consolidate 一起错"。

**Considered Options**

- 保留绝对字符配置（`max_tool_result_chars` 等）：不破坏现有配置面，但窗口一变所有比例失真，且同一个字段同时控制"落盘阈值"与"截断长度"两件事，改哪个都会误伤另一个。
- 一个字段两种写法（`"8%"` 或 `2400`）：迁移成本最低，但配置面永远留着"谁赢"的歧义，WebUI 上填错的形式无法报错。
- 纯百分比、无封顶：随窗口线性放大。32k→200k 会把 MEMORY.md 的写作预算从 2400 token 抬到约 15000 token——"一条记忆该写多少"本来与模型窗口无关，这样会把 Dream 的单次整理和 system prompt 注入成本一起拖垮。
- 集中式 budget 调度器（计量 + 判定 + 动作派发全收进一个模块）：动作执行者分属四个时机、触发条件类型不同（字符数 / 估算 token / provider 真值 token / 文档预算），集中派发等于重新发明 `snip`、`consolidate`、`dream` 的调用约定，还要把无状态的 PreChat 变成有调度状态的。最终只统一**尺子与阈值**，动作就地执行。

**Consequences**

- 两条 breaking change（废弃 `agent.max_tool_result_chars`、`context_window_tokens` 移段），需要 setup 迁移与 WebUI 配置面同步；配合 ADR 0002 的"配置不热重载"，改完必须重启。
- 所有限制**随窗口漂移**是接受下来的代价，靠两头夹住：记忆文档预算取 `min(窗口 × 占比, 绝对封顶)`，数量下限（`min_keep_messages`、`keep_recent_tool_results`）保持绝对条数。没有"这次换模型会让哪些东西变小"的预览。
- 计量明确**两把尺**且都留在接口里：PreChat 用估算轨（`char_per_token_ratio`，冷启动保守取 3.0），consolidate 触发用权威轨（provider 回传 `prompt_tokens`）。不追求统一成一把尺——那会让发请求之前的判定依赖还没拿到的 usage。
- 判定统一、动作分散，两者之间**只有约定没有编译期保证**；将来再加一处自算阈值就是破坏这个决定的方式。
- 术语上 `budget` 一词留给票 11 的**花费配额**（超时、成本上限那一族），容量侧统一叫 **Context pressure**。两者动词不同："什么时候扔什么" vs "什么时候不许再花"，不该共享名词。

**Consequences 补节（2026-11-11，system prompt 组装）**

- 本 ADR 的适用边界显式扩到 **system prompt 组装**：`# Recent History` 不再自带绝对 token 上限，成为锚的**第 7 个消费者**（按 `:3` 的六处机件口径计；生产代码直接调 `.limits()` 的消费点原有 5 个，`MemoryBudgetAnchor` 是 `:17` 已定义的第二个锚、不计入）。此前的 `MAX_HISTORY_TOKENS = 8_000` 与废弃的 `4.0` 换算一并删除：窗口 32k 时该段固定占约 24%，窗口 200k 时降到约 4%，与其余截断动作的漂移不同步。
- 该段预算同构 `:17` 的写法：`min(窗口 × pressure.recent_history_ratio, MAX_RECENT_HISTORY_CHARS)`，字符换算走会话实测 `char_per_token_ratio`、冷启动回退 `3.0`；固定取最近 50 条与 `... (truncated)` 后缀留在锚外。窗口为 `0` 时预算为 `0`，语义是「不知道锚、不截断」，不引入行为反转。
- `:19` 缺的那层保证由 `ContextBuilder::new` 的**必填 `PressureAnchor` 参数**补成编译期保证：漏传即编译失败，`build_system_prompt` 成为可独立驱动的测试面。这**不是** `:12` 否掉的集中式调度器——只把尺子传进去，丢弃 recent history 行的动作仍在本地执行；仍没有第二套调度机件，也不新开 ADR。
- `:14-20` 的**两把尺刻意保留**：`consolidate` 的会话实测 ratio 与触发/强制路径的冷启动默认继续分轨，本次只消除已废弃的 `4.0`，未收敛成一把尺。
