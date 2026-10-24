# Card generation 是一次 Run 结束后的独立模型调用，归属于 channel 层而非 runner

WebUI 需要生成式 UI（json-render 卡片）。我们决定：**卡片不是主 agent 的产出**。一次 Run 结束后，由 channel 层的 `CardService` 用该 Run 的 Run transcript 作为输入、以客户端上报的 Catalog prompt 作为系统提示词，**再发起一次独立的流式模型调用**，把模型吐出的 JSONL patch 逐条编译成 Card spec 并推给前端；卡片与正文是两条并行的产物，互不阻塞、互不污染。

这是对「内联产出」的**有意偏离**。json-render 本身支持 `inline` 模式（模型先写正文、再在 ```spec 围栏里跟 JSONL patch，配 `MixedStreamParser` 解析），一次调用即得文字与卡片，看起来更省。放弃它的理由是它会同时推翻 SlimBot 的四个既有契约：其一，`ContextBuilder` 的 system prompt 是全局统一的，而 `runtime_ctx` 被刻意设计成每轮只生成一次以维持 prompt cache 稳定，按 session 注入一段动态 Catalog prompt 会让缓存全线失效；其二，JSONL patch 会随 assistant 消息落进 session JSONL，从此每一轮上下文和 `Consolidator` 的摘要都要吃这份噪声；其三，Catalog 是 WebUI 的表现层能力，内联会让 CLI、cron、heartbeat 的会话也被迫背上词表；其四，主链路目前没有流式能力，内联等于先把整个 ReAct 循环改成流式，代价远大于收益。独立调用的代价是多一次 token 开销与一次串行延迟，且卡片表述可能与正文不一致——我们接受这两点，因为卡片本就是「锦上添花」，失败可静默跳过。

生成逻辑放在 **channel 层**而非 runner 的 `AfterTurn` hook。`BusResult` 是在 `runner.run()` 返回**之后**才投递到 outbound 的，任何 hook 里同步等待卡片生成，用户就要多盯几秒空白；改成 hook 内 `spawn` 虽可不阻塞，却让 agent 核心知道「表现层有个卡片」，与「卡片只属于 WebUI」的判断相悖。放在 channel 层还有一个好处：`TaskStarted` 事件发出时本轮 user 消息已落库，卡片服务只需在 `TaskStarted` 记一个消息游标、在 `TaskCompleted` 读 `messages[cursor..]`，无需任何消息副本——这正是「Run 期间记录 message list」的意图，只是把副本换成了游标。

卡片增量**不走 `AgentEvent` 广播**。`event_tx` 是容量 256 的 broadcast，lag 时会静默丢事件；patch 流是高频事件，丢一帧就会得到残缺的 spec，而且所有通道的订阅者都会收到与自己无关的卡片事件。我们改为给 `Channel` trait 增加默认 no-op 的 `write_card`，卡片事件经独立 mpsc 到 `ChannelManager` 再路由到具体通道，CLI 等通道继承默认实现直接丢弃。

Catalog prompt **由客户端在连接后上报**，而不在构建期落成文件让服务端读取。两者的词表来源相同，但上报这个动作同时是**能力握手**：它声明「我这个客户端能渲染卡片」，服务端才不会对旧版前端白花一次调用，且 prompt 与前端 registry 天然同源、不存在版本漂移。代价是服务端必须把这段文本当作不可信输入处理（长度上限、不全文落日志），并把「未注册」当作 fail-closed——没有注册就没有卡片。

Card spec 持久化到 `sessions/{chat_id}.cards.jsonl` 侧车文件，以该 Run 末条 assistant 消息的 `timestamp` 为锚，**不写进会话 JSONL**。会话 JSONL 在 consolidation 时会全量重写、消息没有稳定 id，所以锚点只能用 timestamp；而把 spec 混进会话 JSONL 会让 agent 上下文吃到 patch 文本——侧车是唯一同时满足「可回放」与「不污染上下文」的形态。代价是 `/session` 的返回契约要扩字段，且卡片与消息的关联多了一层。

Run 的标识统一为 **`run_id`**，从 `TaskHook` 分配，贯穿生命周期事件、`BusResult` 与卡片事件。代码里原本有一个恒为空串的 `BusResult.task_id`，属于既有契约缺陷；既然这批改动本来就要触碰所有构造点，就顺手把它落实并改名，避免术语表与代码长期互相打脸。

**后续未做**：卡片动作（`emit`）暂不上行到 agent，v1 只允许 `setState` / `visible` 这类本地状态能力；卡片交互驱动新一轮对话需要单独的输入通道、防重入与「用户打字 vs 卡片点击」的区分，届时另立 ADR。
