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

CLI 通道**不需要配置**。运行 `cargo run -- agent` 时自动启用，使用 stdin/stdout 进行交互。CLI 通道不在 `config.channels` 中出现。

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
| `/sse?chat_id=xxx` | GET | SSE 流式输出，接收 Agent 回复、生命周期事件与卡片事件 |
| `/message?chat_id=xxx` | POST | 发送消息到 Agent |
| `/session/{chat_id}` | GET | 会话历史（user/assistant 消息，每条带 `ts` 锚点；assistant 可带 `card` 卡片回放） |
| `/json-render/catalog` | POST | 注册该 chat 的 Catalog prompt（上报即声明「本客户端能渲染卡片」），见「卡片下发」 |
| `/config`、`/traces` | GET | 配置编辑器与 Trace 查看器页面（见 `docs/config.md`、`docs/trace.md`） |

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
3. 服务端经 SSE 连接实时推送，按 `event:` 字段区分三类事件：

| 事件 | data 载荷 | 说明 |
|------|-----------|------|
| `message` | 文本内容 | Agent 最终回复 |
| `agent_event` | `AgentEvent` JSON（`type` 判别） | 生命周期事件（task started/completed 等） |
| `card` | `CardEvent` JSON（`type` 判别） | 卡片生成事件，见「卡片下发」 |

## 卡片下发（Card）

WebUI 在会话气泡下方渲染生成式 UI 卡片（json-render Card spec）。卡片**不是主 agent 的产出**：一次 Run 结束后，channel 层的 `CardService` 以该 Run 的 transcript 为输入、以客户端上报的 Catalog prompt 为系统提示词，另发起一次独立的流式模型调用（决策见 `docs/adr/0005-post-run-card-generation.md`）。

**Catalog 注册（能力握手）：** 前端连接后 `POST /json-render/catalog`，body 为 `{"chat_id", "prompt", "catalog_hash"}`。注册同时声明该 chat 能渲染卡片——**未注册的会话不产生卡片**（fail-closed）。prompt 超长或为空返回 400；卡片服务未接线返回 503。

**卡片事件（SSE `event: card`）：** `CardEvent` 按 `type` 判别（snake_case），均携带 `session_id` 与 `run_id`：

| type | 附加字段 | 语义 |
|------|----------|------|
| `started` | — | 生成开始，前端可展示占位 |
| `delta` | `ops: [...]` | 增量 patch（流式编译产物，前端逐帧合并渲染） |
| `completed` | `spec: {...}` | 权威完整 Card spec，前端以此渲染为最终态 |
| `failed` | — | 生成失败/流停滞/空 spec，前端忽略即可（静默降级） |

**回放：** 完成的 spec 追加到 Card sidecar（`sessions/{chat_id}.cards.jsonl`），以该 Run 末条 assistant 消息的 `timestamp` 为锚，**不写进会话 JSONL**（避免 patch 文本污染 agent 上下文）。`/session/{chat_id}` 读取时按锚点精确挂回 assistant 消息的 `card` 字段；锚点已不存在（被 consolidation 重写）的卡片直接不挂，绝不按索引猜测。

**护栏与配置：** 顶层 `json_render` 配置段控制卡片生成（`enabled` 总开关、`model_override` 指定 provider、`max_tokens` 单次调用输出预算、`timeout_seconds` 单次调用的无进展窗口——任何流式信号到达即续命，思考模型的长推理阶段不会被误杀；真正卡死的流在窗口用满后被掐断）。运行时护栏：并发闸门（超限的 Run 直接跳过、不排队）、停滞取消、进程退出时取消进行中的生成；卡片调用一律以流式、关闭思考、开启 prompt cache 发起（请求体分别带 `stream: true`、`enable_thinking: false` 与 `cache_control`，均不影响主 agent 对话）。每次生成记录 `json_render.generate` span（`json_render.patch_count`、token usage attributes），其下挂 `chat` 子 span 携带 provider 身份、完整请求体与本次调用的 token/缓存命中，可在 Trace 查看器直接审计。

**其他通道：** 卡片事件走独立 mpsc 经 `ChannelManager` 路由，不进 `AgentEvent` 广播；`Channel` trait 的 `write_card` 默认 no-op，CLI/cron/heartbeat 继承默认实现直接丢弃。

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
    async fn write_event(&mut self, event: &AgentEvent) -> Result<()> { Ok(()) }
    async fn write_card(&mut self, event: &CardEvent) -> Result<()> { Ok(()) }
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
