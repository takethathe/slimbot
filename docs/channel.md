# Channel

对应模块：`src/channel/mod.rs` + `src/channel/cli.rs`

## 概述

`Channel` 抽象了 SlimBot 的所有 I/O 通道，支持 CLI、WebUI 等多种输入输出方式。通过工厂模式创建具体通道实例。

## Channel 接口

```rust
#[async_trait]
pub trait Channel: Send + Sync {
    /// 通道唯一标识
    fn id(&self) -> &str;

    /// 通道名称（用于日志和调试）
    fn name(&self) -> &str;

    /// 当前聊天的 chat_id
    fn chat_id(&self) -> &str;

    /// 读取一行用户输入
    async fn read_input(&mut self) -> Result<String>;

    /// 输出最终结果
    async fn write_output(&mut self, result: &BusResult) -> Result<()>;

    /// 输出中间状态（如工具执行进度）
    async fn write_status(&mut self, session_id: &str, state: &TaskState) -> Result<()>;

    /// 输出 Agent 运行时事件（tool_call、assistant_message 等）
    async fn write_event(&mut self, event: &AgentEvent) -> Result<()> { Ok(()) }

    /// 输出卡片事件（WebUI 生成式 UI）。默认 no-op——只有 WebUI 类通道渲染卡片，
    /// CLI、cron、heartbeat 继承默认实现直接丢弃。
    async fn write_card(&mut self, event: &CardEvent) -> Result<()> { Ok(()) }

    /// 准备注入到上下文的额外信息（可选）
    async fn prepare_inject(&self) -> Result<String>;

    /// 生成 Session ID，格式：{channel_id}:{chat_id}
    fn session_id(&self) -> String {
        format!("{}:{}", self.id(), self.chat_id())
    }

    /// 启动通道内部客户端读取循环。由 ChannelManager 在创建后调用。
    fn start(&self, inbound_tx: mpsc::Sender<BusRequest>);

    /// 向客户端发送输出消息。由 ChannelManager 的出站路由器调用。
    /// 默认实现委托给 write_output。
    async fn send_output(&mut self, result: &BusResult) -> Result<()> {
        self.write_output(result).await
    }
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
    event_tx: Option<broadcast::Sender<AgentEvent>>,  // AgentEvent 广播发送端
}
```

### 自动注册内置工厂

`ChannelManager::new()` 自动注册所有内置通道工厂（CLI 等），无需在 main.rs 中手动注册：

```rust
let channel_manager = ChannelManager::new(message_bus);
// 内置工厂已自动注册
```

### `init_from_config`

从配置的 `channels` 数组中读取通道条目，在已注册的工厂中查找对应类型：

```rust
channel_manager.init_from_config(&config.channels).await?;
```

对每个启用的通道：
1. 通过工厂创建通道实例
2. 调用 `channel.start(inbound_tx)` 启动内部读取循环
3. 将通道存入共享 `Arc<Mutex<HashMap>>` 供出站路由使用

### `run` — 出站路由循环

在主线程上阻塞运行，监听出站消息并路由到对应通道：

```
loop:
  result = outbound_rx.recv()
  channel_id = result.session_id.split(':').next()
  channels.get_mut(channel_id).send_output(result)
```

当所有通道关闭、outbound 发送端断开时，此方法返回。

### 卡片事件路由

卡片事件（`CardEvent`，见 `docs/channels.md` 「卡片下发」）**不走 `AgentEvent` 广播**，而是经独立 mpsc 进入 `ChannelManager`：

```rust
channel_manager.attach_card_rx(card_rx); // Gateway 接线时调用，未接线则无卡片链路（如 CLI）
```

`run_with_shutdown` 循环同时监听 outbound 与 card 通道，卡片事件按 `session_id` 的通道前缀路由到对应通道并调用 `write_card`（`dispatch_card` 纯函数，路由决策可直接单测）。不支持卡片的通道由默认 no-op 实现兜底，无需感知卡片机制存在。

## CliChannel

CLI 通道的具体实现：

- `start()`: 使用 `spawn_blocking` 启动阻塞式 stdin 读取循环，不阻塞 tokio 运行时
- `read_input()`: 从标准输入读取一行
- `write_output()`: 将结果打印到标准输出
- `write_status()`: 将状态打印到标准错误
- `prepare_inject()`: 返回空字符串（CLI 无需额外注入）

### `start` 实现

```
spawn_blocking:
  loop:
    显示 prompt
    读取 stdin
    如果 EOF → 退出
    构建 BusRequest
    inbound_tx.send(request)
```

使用 `spawn_blocking` 而非 `tokio::spawn`，因为 `stdin.read_line()` 是阻塞式系统调用，避免阻塞 tokio 工作线程。

### CliChannelFactory

```rust
impl ChannelFactory for CliChannelFactory {
    fn channel_type(&self) -> &str { "cli" }

    fn create(&self, config: &serde_json::Value) -> Result<Box<dyn Channel>> {
        Ok(Box::new(CliChannel::from_config(config)?))
    }
}
```

已在 `ChannelManager::new()` 中自动注册，配置中 `type: "cli"` 的条目会自动创建为 CLI 通道。

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

## 添加新通道类型

1. 实现 `Channel` trait（包括 `start` 和 `send_output`）
2. 实现 `ChannelFactory` trait，返回类型标识
3. 在 `ChannelManager::new()` 中注册新工厂（或在外部调用 `register_factory`）
4. 在配置文件的 `channels` 数组中添加对应类型的条目

```json
{
  "channels": [
    { "type": "webui", "enabled": true, "config": { "port": 8080 } }
  ]
}
```

## 配置结构

```json
{
  "type": "cli",
  "enabled": true,
  "config": {}
}
```

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `type` | string | 是 | 通道类型，匹配工厂的 `channel_type()` |
| `enabled` | bool | 否 | 是否启用，默认 `true` |
| `config` | object | 否 | 通道特定配置，任意 JSON |
