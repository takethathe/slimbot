# 飞书通道用非官方 Rust SDK 原生实现，而非 Node 中转或从零手写

v1 的首个 IM 通道选定飞书（Feishu），并要求流式卡片、图片 / 附件、飞书 markdown 全部进 v1。约束是：飞书官方 Channel SDK 只覆盖 Node / Python / Go / Java，**没有 Rust**；而 slimbot 是单二进制、本地优先的 Rust 程序。长连接事件帧是 protobuf 分帧。决策是新增 `src/channel/feishu.rs` 作为原生 `Channel`，坐社区 crate `larksuite-oapi-sdk-rs`（非官方、Tokio-native、对齐官方 Go SDK 契约），**只启用 `channel` feature**（传递 `messaging` + `ws`，不开 `axum`）、精确锁版本，进程内直接复用其 WS 长连、事件归一化、策略 / 去重、CardKit 流式与媒体上传下载。这放弃了官方 SDK 的协议保真度，换到的是：不破坏单二进制 / 本地优先、不在 v1 重造 protobuf WS + 流式卡片 + 媒体这套已被 SDK  spare 的轮子。

**Considered Options**

- **本地 Node 中转官方 `@larksuite/channel`**：跑一个 localhost Node 进程用官方 SDK 连飞书，slimbot 与之走 stdio / 本地 WS / HTTP 通信。优点是协议保真最高（官方 SDK）、要的能力全现成；缺点是引入 Node 运行时依赖 + 双进程守护 + 一道 IPC 序列化边界，破坏单二进制与本地优先，分发变重。列为回退方案（plan B）。
- **从零手写 WS + REST（参考 SDK 源码）**：零外部 crate 依赖、最大掌控、无供应链押注；但要自己实现 protobuf 分帧的握手 / ACK / 重连 + CardKit 流式 + 媒体上传下载 + markdown→卡片 schema，成本最高、最慢到 v1。飞书「5 分钟接入」的前提是用 SDK，从零不成立。
- **选定的 Rust 原生 + 社区 `larksuite-oapi-sdk-rs`**：进程内现成拿到 WS / 归一化 / 策略 / 流式 / 媒体，保持单二进制；代价是把 v1 押在一个单人维护的 0.3.x 非官方 crate 上，用隐私 / 供应链护栏对冲。

**Consequences**

- 源码审查（`Cargo.toml` / `cache.rs` / `error.rs`）显示 crate 架构隐私友好：无遥测外发；token 默认只在内存（`LocalCache`，TTL 逐出、不落盘）；DPoP 私钥应用自持；`LarkError` 构造上不塞请求体 / 密钥。
- 依赖护栏：精确锁版本、只开 `channel`、禁 `axum`（走 WS 不需 webhook handler，缩小网络面）；标记为「安全敏感依赖」；`cargo audit` 纳入提交前检查；升级前重新 diff 其 tracing / error 面。
- 隐私护栏：通道适配器永不记录消息正文 / `open_id` / `tenant_key` / `app_secret` / token；入站媒体只按飞书 file-key 经 im API 下载、不在通道路径拉任意 URL（SSRF 面归零，任意抓取留给 `web_fetch` 工具）。
- 部署约束：企业自建应用 + 单条 WS 长连，且因飞书集群模式「多 client 只有随机一个收事件」，**一个 gateway 进程必须是该 app 的唯一连接持有者**——这既是正确性约束也是隐私约束（事件不跨进程 fan-out）。
- 供应链风险：crate 断维护或落后协议变更时需 fork / vendor；残留一项实现期核验——逐 call-site 确认其 `tracing` 未把消息正文 / PII 打进 `debug!`/`trace!`，若有则在 slimbot 侧加 filtering layer 兜住。
- 重开条件：若该 crate 无法满足流式 / 媒体 / markdown 需求，或安全审查发现不可接受的隐私泄露 → 回退到机制 N（本地 Node 中转官方 SDK）。
