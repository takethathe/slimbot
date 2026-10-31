# subagent 用独立 Runner 与工具增量，而不是给主 Runner 动态加工具

要的是一个「带工具的执行体」：主 loop 把一件事委托出去，它用自己的工具集在自己的会话里跑完，再把结果投回来。最省事的做法是给主 `AgentRunner` 加一个 `switch_tools(names)`，跑完再切回去。我们选择**每次运行从 catalog 重新装配一个 `ToolManager` 和一个 `AgentRunner`**（`DreamService` 已是同型先例），理由是工具集在构造期就冻结了：`ToolManager::register` 会同时写工具表和缓存好的 `openai_tools`，而 `Arc<ToolManager>` 被主 loop 的并发 run 共享（`src/tool.rs:112-198`）。运行时增删工具等于改一个别人正在读的共享结构，还会让 system prompt 的 `tools` 段在每个 run 里抖动、把 prompt cache 打穿。重新装配的代价是每次运行重新构造十来个轻量对象，换取的是「工具集只属于这一次 run」这条硬边界。

`tools` 字段写的是**增量**而不是全集：默认集由 `config.tools` 演进，若 agent 声明全集，用户往配置里加一个工具就得改所有 agent 定义。同理，增量的边界必须挡住两样东西——`cron` 与 `subagent` 本身。挡住 `cron` 是因为子任务不该能绕过触发器模型私建定时器；挡住 `subagent` 是因为递归会让「并发上限 3」变成一个可以自己乘方的数字。两者都按「非法声明」处理，与「声明了未注册的工具」同构：加载期剔除该 agent 并记 `error!`，调用期再拒一次并回显可用工具全集。宁可让一个写错的 agent 直接不可用，也不要让它带着半个工具集跑起来——后者的失败发生在任务中间，而且看起来像模型不会用工具。

会话归 memory：`subagent:<agent>:<run_id>`，一次性，不进 per-session 串行队列（`run_id` 唯一，天然串行）。它**不落盘、不留痕**，跑完连 session 一起删掉。为此要动两处既有代码：`SessionManager` 是纯内存 `HashMap` 却**没有删除接口**，需要新增 `drop_session`；`AgentRunner` 的失败分支会无条件 `persist`（`cancelled_result` 里就有一次），subagent 路径必须能把落盘关掉。留痕这条本来打算做（`agents/runs/<run_id>.md`），后来砍掉了：留痕的唯一用途是演进 agent 的提示词，而现在没有任何机制消费这份记录——写下来只会在磁盘上积一层没人读的东西。`deliver=false` 的 run 结果进 `tracing` 就够。

结果回流是这层的对外出口，形状必须固定：头行结构化（`agent=` / `run_id=` / `ok=` / `elapsed=`），正文是结果，末行是指令（「总结给用户：做了什么、发现了什么、有什么需要他决定」）。头行让模型知道是谁跑完了、跑了多久；末行让模型总结而不是复述，也不会把这段当成用户原话。这两句就是「结果要清晰到模型能直接给出合适反馈」的实现形式。

trace 上 subagent 是**独立 root span**（`subagent.run`），不挂在发起方 turn 的 span 下面。它是异步的，会活过发起它的那一轮；挂成 child 必然产生「子 span 比父 span 晚结束」的畸形 trace。

**代价**（有意接受）：每次运行重装工具；agent 之间不共享会话，所以 `task` 必须自带全部上下文（这是隔离换来的便宜）；没有 `subagent_status` / `subagent_cancel`，因为取消需要 runner 侧协作式中止，而目前没有。同名 agent 允许并发：把「同名只许一个」当成正确性约束是多虑——两个 `news` 跑两个不同任务本来就没有冲突。
