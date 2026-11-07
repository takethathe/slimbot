# Feishu 通道（票 27：p2p 文本往返；票 28：流式卡片 + 图片/文件收发）

slimbot 的首个 IM 通道。owner 在飞书私聊里对 bot 说话，一条消息触发一次 ReAct
run，答案回到同一个 chat。长答案流式更新进一张原位刷新、终态落定的卡片；owner
可收发图片与文件。决策见票 09 与 `docs/adr/0017`。

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
| `FeishuSender`（接缝） | 出站发送；真实实现 `SdkSender` 走 `channel_messaging()` 的 `send_text` / `send`（图片 / 文件 / 卡片）/ `edit_card`（卡片原位更新） |
| `MediaDownloader`（接缝） | 入站附件按 file-key 拉字节；真实实现 `SdkDownloader` 走 `im` resource API，无 URL 参数（防 SSRF） |

连接生命周期：`init()` 只创建通道、**不**建连；`start_with_scheduler`（gateway
启动时）才打开唯一长连接，SDK `start` 自带重连。该任务用 `tokio::select!` 把
`pump.start()` 与 gateway 的 channel-shutdown broadcast 并列：收到 shutdown 即
drop 掉 future、关掉底层 WebSocket，`ChannelManager::shutdown` 能正常 join，
不会卡到超时并跳过 session/memory 落盘。出站 `send_text` 走 SDK 客户端，请求
超时从默认 30s 收紧到 10s——出站发送在 ChannelManager 共享锁内执行，慢调用不能
拖住其它通道。

## 隐私护栏（ADR 0017）

- 适配器日志**永不**携带：消息正文、`open_id`、`tenant_key`、`app_secret`、
  token，以及任何媒体句柄（file-key）、原始文件名、本地媒体路径。忽略分支只落固定文字（如
  "dropped a DM from a non-owner"），SDK 错误只经 `error_class` 落**变体名 +
  数字错误码**（`LarkError` 的 Display 会带服务端自由文本，可能回显请求参数，故绝不整条落盘）。
- 所有日志走一个入口 `note`：先经全局宏，`cfg(test)` 下同时镜像进内存 capture，
  守护测试据此断言——不碰进程级 logger 单例（其它 lib 测试会 init 它，直接落文件
  会竞争初始化顺序）。
- 守护测试：`src/channel/feishu.rs` 的
  `test_round_trip_logs_never_carry_pii`（文本往返）与
  `test_media_logs_never_carry_handles_names_or_paths`（媒体收发）用唯一
  marker 走一遍后扫描 capture（两个 capture 测试经 `CAPTURE_GUARD` 串行，避免
  进程级 capture static 被并发测试抽干）；`tests/feishu_integration.rs` 锁定
  配置错误不回显 app_secret。
- 上游 crate 的 `tracing` 逐 call-site 核验、以及把 SDK tracing 桥进 `note`（只
  落失败类别）属于运维跟进项（见 `docs/TODO.md` 飞书节）。

## 流式卡片与媒体（票 28）

- **流式卡片**：`streaming` 默认开启（仅显式 `"streaming": false`/`false` 关，
  保留纯文本往返）。开启时一个 run 对应一张卡片：`TaskStarted` 事件建档（空体、
  `streaming_mode=true` 显输入中），每条 `AssistantMessage` 事件把当前全文刷进
  同一张卡（`edit_card` 原位更新，不刷屏），`TaskCompleted`/`TaskFailed` 以
  `streaming_mode=false` 定稿并记下 `run_id`；随后的 `BusResult` 在 `write_output`
  按 `run_id` 去重，不再另发文本。建卡或更新失败即回退：该 run 定稿改走普通文本
  答案（卡片只是显示层，坏了不丢答案）。
- **入站媒体**：图片按 file-key 下载后以多模态 `Content::Image`（base64）进
  run 上下文；非图片文件落盘到 `{workspace}/media/feishu/`（文件名只取最后
  一个 path 分量，防目录穿越），以 `[attached file saved to: …]` 路径行引用。
  **只按飞书 file-key 拉取**：URL 形状的句柄在任何请求前即被拒绝（无 SSRF
  探测面）。
- **出站媒体**：`send_file` 工具把本地文件投递到 media 队列，通道
  `write_media` 交给 SDK 上传发送（caption 先作为单独 text 消息发出）；同样
  拒绝 URL 形状路径。
- `docs/TODO.md` 飞书节记录运维跟进（上游 tracing call-site 核验等）。
