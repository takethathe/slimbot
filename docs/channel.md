# Channel

对应模块：`src/channel/mod.rs` + `src/channel/webui.rs` + `src/channel/feishu.rs`

## 概述

`Channel` 抽象了 SlimBot 的所有 I/O 通道，支持 CLI（由 `ShutdownHandle::run_main_thread_loop` 在主线程上阻塞读 stdin 驱动，不是 `Channel` 实现）、WebUI、飞书。通过工厂模式创建具体通道实例。

## Channel 接口

```rust
#[async_trait]
pub trait Channel: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn chat_id(&self) -> &str;

    async fn read_input(&mut self) -> Result<String>;
    async fn write_output(&mut self, result: &BusResult) -> Result<()>;
    async fn write_status(&mut self, session_id: &str, state: &TaskState) -> Result<()>;
    async fn prepare_inject(&self) -> Result<String>;

    fn session_id(&self) -> String {
        format!("{}:{}", self.id(), self.chat_id())
    }

    /// 启动通道内部读取循环，由 ChannelManager 在创建后调用。
    fn start(&self, inbound_tx: tokio::sync::mpsc::Sender<BusRequest>);

    /// 走 IoScheduler 的启动入口；默认给一个空 IoHandle。
    fn start_with_scheduler(&self, scheduler: &IoScheduler) -> IoHandle { .. }

    /// 向客户端发送输出；默认委托 write_output。
    async fn send_output(&mut self, result: &BusResult) -> Result<()> { self.write_output(result).await }

    /// AgentEvent / 卡片 / 文件投递，默认全是 no-op。
    async fn write_event(&mut self, event: &AgentEvent) -> Result<()> { Ok(()) }
    async fn write_card(&mut self, event: &crate::card::CardEvent) -> Result<()> { Ok(()) }
    async fn write_media(&mut self, delivery: &crate::message_bus::MediaDelivery) -> Result<()> { Ok(()) }
}
```

## ChannelFactory

```rust
pub trait ChannelFactory: Send + Sync {
    /// 返回该工厂支持的通道类型标识
    fn channel_type(&self) -> &str;

    /// 从配置创建通道实例
    fn create(&self, config: &serde_json::Value) -> Result<Box<dyn Channel>>;
}
```

## ChannelManager

```rust
pub struct ChannelManager {
    channels: Arc<Mutex<HashMap<String, Box<dyn Channel>>>>,
    factories: HashMap<String, Box<dyn ChannelFactory>>,
    message_bus: Arc<MessageBus>,
    outbound_rx: Mutex<Option<mpsc::Receiver<BusResult>>>,   // 单消费者，run 时取走
    card_rx: Mutex<Option<mpsc::Receiver<CardEvent>>>,       // attach_card_rx 接
    media_rx: Mutex<Option<mpsc::Receiver<MediaDelivery>>>,  // attach_media_rx 接
    config: Arc<Config>,
    event_tx: Option<broadcast::Sender<AgentEvent>>,
    // io_handles / io_scheduler 略
}
```

### 工厂注册

工厂由装配层显式注册，`ChannelManager::new()` 本身不注册任何通道：

```rust
channel_manager.register_factory("feishu", Box::new(FeishuChannelFactory::new(..)));
channel_manager.register_webui_factory(startup);   // 一份 WebuiStartup，见下
```

### `init`

从配置的 `channels` 映射中读取通道条目，在已注册的工厂中查找对应类型：

```rust
channel_manager.init().await?;
channel_manager.start_channels().await;
```

`init` 对每个启用条目：
1. 通过工厂 `create(&Value)` 创建通道实例
2. 将通道存入共享 `Arc<Mutex<HashMap>>` 供出站路由使用

`start_channels` 再逐个调用 `start_with_scheduler`，把真正起服务的通道的 `IoHandle` 收进 `io_handles`。

### `run_with_shutdown` — 出站路由循环

gateway 里以 `tokio::spawn` 运行，监听出站消息并路由到对应通道：

```
loop:
  result = outbound_rx.recv()
  channel_id = result.session_id.split(':').next()
  channels.get_mut(channel_id).send_output(result)
```

收到 `shutdown` 广播或所有发送端断开时返回。

### WebUI 启动快照（`WebuiStartup`）

WebUI 需要的每个运行时对象都写在同一份纯数据里，只经 `register_webui_factory` → `WebuiChannelFactory::create` 一条路径消费：

```rust
pub struct WebuiStartup {
    pub session_manager: SharedSessionManager,
    pub shutdown: broadcast::Sender<()>,   // 关停广播
    pub events: broadcast::Sender<AgentEvent>,
    pub trace_paths: Vec<String>,
    pub trace_startup_ms: Option<i64>,
    pub config_path: String,               // 启动快照，不可热更新（ADR 0002）
    pub card: Option<Arc<CardService>>,
}
```

- `ChannelManager::register_webui_factory(startup)` 只收这一个参数；`WebuiChannelFactory::new(message_bus, startup)` 只有两个参数；通道由 `WebuiChannel::with_wiring(message_bus, startup)` 一次装配，`AppState` 持有同一份快照。新增一个 channel-side 参数 = 往 `WebuiStartup` 加一个字段。
- `config_path` 只是路径：Config editor 的三个 handler 每次自己读盘（`load_for_preview` / `save`），所以保存的改动在编辑器里立刻可见、在进程里要等重启。
- `shutdown` 由通道在创建时 `subscribe()` 出接收端，与 `events`、`session_manager` 等一起代表「这个进程里的 WebUI 运行环境」。

### 卡片事件路由

卡片事件（`CardEvent`，见 `docs/channels.md` 「卡片下发」）**不走 `AgentEvent` 广播**，而是经独立 mpsc 进入 `ChannelManager`：

```rust
channel_manager.attach_card_rx(card_rx); // Gateway 接线时调用，未接线则无卡片链路（如 CLI）
```

`run_with_shutdown` 循环同时监听 outbound 与 card 通道，卡片事件按 `session_id` 的通道前缀路由到对应通道并调用 `write_card`（`dispatch_card` 纯函数，路由决策可直接单测）。不支持卡片的通道由默认 no-op 实现兜底，无需感知卡片机制存在。

## CLI 通道

CLI 没有独立的 `Channel` 实现：`agent` 模式下 `AgentLoop` 的入站循环照常运行，但输入输出由主线程的 `ShutdownHandle::run_main_thread_loop` 负责——阻塞读 stdin、构 `BusRequest`、`try_send` 进 inbound，遇到 channel-tier 命令（`/quit`）就广播关停。所以 `channels` 里没有 `cli` 条目，`ChannelManager` 也没有 CLI 工厂。

## WebUI 通道的边界与护栏（ADR 0010）

WebUI 是**纯本机入口**：无认证、无 token，本机即 owner。

- **绑定**：只允许 loopback（`127.0.0.1` / `localhost` / `::1`），默认 `127.0.0.1`；其他值在 `WebuiChannel::from_config` 即失败（`ChannelManager::init` 向上抛出，启动失败），`start_server` 启动前再校验一次。
- **四条全局中间件**（`guard_local_request`，覆盖整张路由表，不逐 handler）：

| 顺序 | 控制 | 适用 | 失败响应 |
|---|---|---|---|
| 1 | `Host` 必须是 loopback | 所有请求 | `421 Misdirected Request`（封死 DNS rebinding） |
| 2 | 同源 `Origin`（缺失即拒） | 变更请求（POST/PUT/PATCH/DELETE） | `403` |
| 3 | `Content-Type: application/json` | 变更请求 | `415`（借浏览器 preflight 挡住 `no-cors` 简单请求） |
| 4 | `Sec-Fetch-Site: same-origin` | 变更请求 | `403` |

不返回任何 CORS 头（零跨源）。因此脚本与集成测试调变更端点时必须显式带 `Origin`、`Content-Type`、`Sec-Fetch-Site` 三个头。

端点保护矩阵：

| 端点 | 方法 | 保护 |
|---|---|---|
| `/`、`/common.css`、`/traces`、`/config` | GET（静态页） | Host |
| `/chats`、`/session/*chat_id`、`/traces/api/*`、`/config/api/state` | GET（只读） | Host |
| `/sse?chat_id=` | GET | Host（跨源读取由 SOP 阻挡） |
| `/message?chat_id=`、`/chats`、`/config/api/update`、`/json-render/catalog` | POST | Host + Origin + `application/json` + `Sec-Fetch-Site` |

本机任意进程仍可全权操作 WebUI（含写配置）——显式接受，不做缓解。

## Feishu 通道

首个 IM 通道（票 27，仅 Gateway 模式）：企业自建应用 + 单条 WebSocket 长连接，
`owner_open_id` 配置绑 owner，只处理 p2p 文本往返；`Channel` trait 未改动，
经 `register_factory("feishu", …)` 在 `src/gateway.rs` 装配。网络隔离在
`FeishuTransport` / `FeishuSender` 两个接缝 trait 后，决策逻辑离线可测。
详见 `docs/feishu.md`（隐私护栏见 ADR 0017）。

## 添加新通道类型

1. 实现 `Channel` trait（包括 `start` 和 `send_output`）
2. 实现 `ChannelFactory` trait，返回类型标识
3. 在装配处注册工厂：`ChannelManager::new()` 的 `factories` 是空表，`register_factory(type_name, factory)` 加普通通道、`register_webui_factory(startup)` 加 WebUI（`src/gateway.rs::assemble_gateway` 各调一次）
4. 在配置文件的 `channels` 映射里添加同名键的条目

```json
{
  "channels": {
    "webui": { "enabled": true }
  }
}
```

## 配置结构

`channels` 是 `HashMap<String, ChannelConfig>`，键就是工厂的类型名（`webui` / `feishu`）；每个条目的 `enabled` 决定是否实例化，其余键落在 `extra` 里由通道自己解释。CLI 不在 `channels` 里——它是进程的默认运行模式（`main` 走 `run_cli_agent`），没有条目也没有工厂。

```json
{
  "webui": {
    "enabled": true
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `type` | string | 是 | 通道类型，匹配工厂的 `channel_type()` |
| `enabled` | bool | 否 | 是否启用，默认 `true` |
| `config` | object | 否 | 通道特定配置，任意 JSON |
