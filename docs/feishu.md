# Feishu 通道（票 27：p2p 文本往返）

slimbot 的首个 IM 通道。owner 在飞书私聊里对 bot 说话，一条消息触发一次 ReAct
run，答案回到同一个 chat。决策见票 09 与 `docs/adr/0017`。

## 形态

- **企业自建应用 + 单条 WebSocket 长连接**：坐社区 crate
  `larksuite-oapi-sdk-rs`（锁定 `=0.3.13`，只开 `channel` feature，`axum` 关闭，
  标记为安全敏感依赖）。长连接由 **gateway 进程独占**（唯一连接持有者），
  CLI 模式不注册该通道，第二个实例不可能双份处理同一条消息。
- **`Channel` trait 原样复用**，未做任何修改；注册走既有
  `ChannelManager::register_factory` + `init()` 装配（`src/gateway.rs`）。

## 身份与会话

- `channels.feishu.owner_open_id` 以配置绑定 owner：
  - owner 私聊 → 以 owner 权限（`Author::Owner`）进入 `MessageBus`，启动 run；
  - 非 owner 私聊 → 静默忽略（不启动 run、不回复）；
  - 群组消息 → v1 整体关闭，直接丢弃（即便来自 owner）。
- session id 两段式 `feishu:{chat_id}`（如 `feishu:oc_xxx`），不做 per-user
  分叉；同一 chat 的多轮对话经既有 `SessionManager` 累积到同一 JSONL 会话。

## 配置

见 `docs/config.md` 的「Feishu 通道配置」。`app_secret` 推荐写
`${secret:...}` 占位符，走 keychain → env → 0600 文件的解析链；通道密钥
命名空间为 `slimbot.channel.<channel>.<field>`（`src/secrets.rs` 的
`resolve_channel_secrets`，裸名 `${secret:app_secret}` 自动补全为
`slimbot.channel.feishu.app_secret`）。

## 结构与可测试性

`src/channel/feishu.rs` 把网络隔离进两个接缝 trait，其余决策逻辑全部离线可测：

| 部件 | 职责 |
|------|------|
| `classify_inbound`（纯函数） | 依据 chat_type / sender / owner 判定 `StartRun` / `IgnoreNonOwner` / `IgnoreGroup` / `IgnoreEmpty` |
| `InboundRouter` | 把判定结果落到 `MessageBus`（构造两段式 session id + `BusRequest::owner`） |
| `FeishuTransport`（接缝） | 入站长连接；真实实现 `SdkTransport` 内部跑 SDK 的 `Channel`（policy 为 defense-in-depth：DM allowlist 只放 owner、不要求 @） |
| `FeishuSender`（接缝） | 出站发送；真实实现 `SdkSender` 走 `channel_messaging().send_text` |

连接生命周期：`init()` 只创建通道、**不**建连；`start_with_scheduler`（gateway
启动时）才打开唯一长连接，SDK `start` 自带重连。该任务用 `tokio::select!` 把
`pump.start()` 与 gateway 的 channel-shutdown broadcast 并列：收到 shutdown 即
drop 掉 future、关掉底层 WebSocket，`ChannelManager::shutdown` 能正常 join，
不会卡到超时并跳过 session/memory 落盘。出站 `send_text` 走 SDK 客户端，请求
超时从默认 30s 收紧到 10s——出站发送在 ChannelManager 共享锁内执行，慢调用不能
拖住其它通道。

## 隐私护栏（ADR 0017）

- 适配器日志**永不**携带：消息正文、`open_id`、`tenant_key`、`app_secret`、
  token。忽略分支只落固定文字（如 "dropped a DM from a non-owner"），SDK 错误
  只经 `error_class` 落**变体名 + 数字错误码**（`LarkError` 的 Display 会带服务端
  自由文本，可能回显请求参数，故绝不整条落盘）。
- 所有日志走一个入口 `note`：先经全局宏，`cfg(test)` 下同时镜像进内存 capture，
  守护测试据此断言——不碰进程级 logger 单例（其它 lib 测试会 init 它，直接落文件
  会竞争初始化顺序）。
- 守护测试：`src/channel/feishu.rs` 的
  `test_round_trip_logs_never_carry_pii` 用唯一 marker 走一遍完整往返后扫描
  capture；`tests/feishu_integration.rs` 锁定配置错误不回显 app_secret。
- 上游 crate 的 `tracing` 逐 call-site 核验、以及把 SDK tracing 桥进 `note`（只
  落失败类别）属于运维跟进项（见 `docs/TODO.md` 飞书节）。

## 范围外（票 28）

流式卡片原位更新、图片/文件收发、markdown 映射、入站媒体只按飞书 file-key
拉取（不拉任意 URL，防 SSRF）。当前实现对这些留了钩子：`write_status` /
`write_card` 为 no-op，`InboundMessage.message_id`（回复目标）与空正文的
`IgnoreEmpty` 分支已就位。
