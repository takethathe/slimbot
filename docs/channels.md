# 通道（Channel）

通道是 slimbot 的输入/输出抽象层，屏蔽了具体的通信协议差异。

## 架构

```
CLI 模式:
  主线程 (stdin) → AgentLoop ←→ MessageBus ←→ ChannelManager → Channel

Gateway 模式:
  Cron/Heartbeat → AgentLoop ←→ MessageBus ←→ ChannelManager → WebUI Channel (HTTP/SSE)
```

## 运行模式

### CLI 模式

CLI 通道**不需要配置**。运行 `cargo run --` 时自动启用，使用 stdin/stdout 进行交互。CLI 通道不在 `config.channels` 中出现。

### Gateway 模式

运行 `cargo run -- gateway` 时启动：
- Cron 定时调度服务
- Heartbeat 定期检查服务
- `config.channels` 中所有 `enabled: true` 的通道

Gateway 模式**不启动 CLI 通道**。

## 通道配置

通道在 `config.json` 的 `channels` 字段中配置，格式为 **键值映射**。**配置键即为通道类型**，无需 `type` 字段。

```json
{
  "channels": {
    "webui": {
      "enabled": true,
      "host": "127.0.0.1",
      "port": 8080
    }
  }
}
```

| 通道类型 | 配置键 | 描述 |
|----------|--------|------|
| `webui` | `"webui"` | Web 界面，HTTP + SSE |

每个通道条目必须包含 `enabled`（bool，默认 `true`），其余字段由具体通道类型定义。

## 内置通道

### WebUI Channel

基于 axum 的 HTTP 服务器，支持浏览器访问和实时对话。

**启动方式：** 在 `config.channels` 中添加 `"webui"` 条目，启动 Gateway 模式即可。

**HTTP 端点：**

| 端点 | 方法 | 描述 |
|------|------|------|
| `/` | GET | 对话界面（内置 index.html） |
| `/chats` | GET | 列出所有会话 ID |
| `/chats` | POST | 创建新会话，返回 chat_id |
| `/sse?chat_id=xxx` | GET | SSE 流式输出，接收 Agent 回复 |
| `/message?chat_id=xxx` | POST | 发送消息到 Agent |

**配置字段：**

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `host` | string | `"0.0.0.0"` | 监听地址 |
| `port` | integer | `8080` | 监听端口（0 = 系统分配） |

**使用示例：**

```bash
# 配置
cat >> ~/.slimbot/config.json << 'EOF'
{
  "channels": {
    "webui": { "enabled": true, "host": "127.0.0.1", "port": 8080 }
  }
}
EOF

# 启动 Gateway
cargo run -- gateway

# 启动后日志会输出监听地址：
# [webui] Listening on http://127.0.0.1:8080
```

**SSE 消息流：**
1. 客户端访问 `/sse?chat_id=xxx` 建立 SSE 连接
2. Agent 通过 `/message?chat_id=xxx` 端点接收用户消息
3. Agent 回复通过 SSE 连接实时推送到客户端

## 扩展通道

新通道可通过实现 `Channel` trait 添加：

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
    fn start(&self, inbound_tx: mpsc::Sender<BusRequest>);
    fn start_with_scheduler(&self, scheduler: &IoScheduler) -> IoHandle;
    async fn send_output(&mut self, result: &BusResult) -> Result<()>;
}
```

实现 `ChannelFactory` trait 用于创建通道实例：

```rust
pub trait ChannelFactory: Send + Sync {
    fn channel_type(&self) -> &str;
    fn create(&self, config: &serde_json::Value) -> Result<Box<dyn Channel>>;
}
```

在 `ChannelManager` 中注册工厂：

```rust
channel_manager.register_factory("my_channel", Box::new(MyChannelFactory));
```

## Session ID 格式

所有通道的 Session ID 格式为 `{channel_id}:{chat_id}`，例如 `webui:abc123`。
