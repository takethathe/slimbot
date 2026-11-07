# trace 默认开启、正文不脱敏、覆盖到所有 provider 调用

`docs/trace.md` 的机制早已存在（ADR 0001），但一个刚跑完 `setup` 的机器上 `/traces` 永远是空的：`tracing.enabled` 默认 `false` 且 `backends` 默认空数组。同时「所有模型请求和回复」并不成立——会话压缩的两次摘要调用根本没有 span，卡片生成的流式调用记了请求体却不记回复正文。用户的目标是「在本地看到所有模型请求和回复」，而这需要三个都会让未来的读者停下来问「为什么」的选择同时成立。

决策是：**新建配置默认开启 trace**（`setup` 产物 `enabled: true` + 一条 file backend + 一条默认关闭的 otel backend）；**正文不脱敏、不截断**（trace 文件与 session JSONL 同级敏感，脱敏会毁掉排障价值）；**覆盖扩到所有 `provider.chat` / `chat_stream` 调用**（consolidation 得到 `consolidation → chat ×N` 的父子结构，卡片 `chat` span 补上 `gen_ai.response.content`）。`TraceConfig::default()` 本身保持关闭：默认只体现在 `setup` 产物里，升级不会在用户背后开始写盘。本地查看端不新增常驻 collector——现有 WebUI Trace viewer 是主视图，OTel 只保留「改一条配置即可切过去」的出口。

**Considered Options**

- **保持默认关闭，只补文档**：改动最小、隐私姿态不变；代价是「本地能看到」仍然取决于用户先读文档再改配置，而这正是本次要消除的摩擦。
- **默认开启但脱敏（截断 / 哈希 / 只留结构）**：隐私更安全、落盘体积更小；代价是排障时看不到模型到底收到了什么，trace 退化成耗时表——而这恰恰是这个个人 agent 最难排查的部分（prompt 里混着记忆注入、工具结果与裁剪边界）。
- **默认开启 + 常驻 collector + OpenInference/Phoenix 属性映射**：生态最完整、UI 最漂亮；代价是单机多一个常驻进程与一套映射维护，且同一批 span 已有专用 viewer，会把同样的 JSON 看第二遍。本机也没有容器运行时。
- **选定方案**：默认开 + 全量正文 + 覆盖全 provider 调用 + OTel 只留接缝。

**Consequences**

- trace 文件从此按「会话正文」对待：文档必须写明勿外传；体积靠 gateway 每次启动截断（`reset_on_start`）与既有 50 MiB 告警兜底，不新增轮转。
- 既有配置不受影响：只有重跑 `setup`（或手工改配置）才会打开，`TraceConfig` 的默认值仍是关闭，避免升级即写盘。
- consolidation 的父 span 是 slimbot 自定义层，故不设 `gen_ai.operation.name`，与 `iteration` 同口径；`chat` 子 span 仍完整遵守 GenAI 语义约定。
- OTLP HTTP 导出器对**显式传入的 endpoint 原样使用**（不像环境变量那样补信号路径），因此 `http://host:4318` 会 POST 到 `/` 被标准 collector 拒绝；`OtelBackend` 在 endpoint 无路径时补 `/v1/traces`，这条修复由端到端测试钉住。
- 不推翻 ADR 0001（自研 span 模型）与 ADR 0018（不建 OTel metrics）；metrics、采样、轮转、正文脱敏、Phoenix 专属属性、`/traces` 的 Chat 面板仍在 scope 外。
