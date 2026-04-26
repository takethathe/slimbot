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
    
    /// 准备注入到上下文的额外信息（可选）
    async fn prepare_inject(&self) -> Result<String>;
    
    /// 生成 Session ID，格式：{channel_id}:{chat_id}
    fn session_id(&self) -> String {
        format!("{}:{}", self.id(), self.chat_id())
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

工厂模式允许在不修改核心代码的情况下注册新通道类型。

## ChannelManager

```rust
pub struct ChannelManager {
    channels: Vec<Box<dyn Channel>>,
    message_bus: std::sync::Arc<MessageBus>,
}
```

### `init_from_config`

从配置的 `channels` 数组中读取通道条目，在已注册的工厂中查找对应类型，创建通道实例：

```rust
channel_manager.init_from_config(&config.channels, &[
    Box::new(CliChannelFactory),
    // 未来可添加更多工厂
])?;
```

### `run` — I/O 循环

为每个通道启动独立的 `tokio::spawn` 任务：

```
对每个通道：
  ├─ 创建 status channel (tokio::sync::mpsc, 容量 32)
  ├─ 创建 TaskHook，绑定 status channel
  ├─ 启动后台任务：监听 TaskState 变更 → 打印到 stderr
  │     [channel_name] [session_id] Running - iteration 1
  │     [channel_name] [session_id] Completed
  │     [channel_name] [session_id] Failed - error message
  │
  └─ 启动 I/O 循环：
        loop:
          input = channel.read_input()
          inject = channel.prepare_inject()
          request = BusRequest { session_id, input, inject, hook }
          result = MessageBus.send(request)   // 在 spawn 中执行
          channel.write_output(result)
```

所有通道**并发运行**，互不阻塞。主函数进入 `sleep` 无限等待。

## CliChannel

CLI 通道的具体实现：

- `read_input()`：从标准输入读取一行
- `write_output()`：将结果打印到标准输出
- `write_status()`：将状态打印到标准错误
- `prepare_inject()`：返回空字符串（CLI 无需额外注入）

### CliChannelFactory

```rust
impl ChannelFactory for CliChannelFactory {
    fn channel_type(&self) -> &str { "cli" }
    
    fn create(&self, config: &serde_json::Value) -> Result<Box<dyn Channel>> {
        // 从 config 中读取配置（可选）
        // 创建 CliChannel
        Ok(Box::new(channel))
    }
}
```

注册到 `ChannelManager` 后，配置中 `type: "cli"` 的条目会自动创建为 CLI 通道。

## 添加新通道类型

1. 实现 `Channel` trait
2. 实现 `ChannelFactory` trait，返回类型标识
3. 在 `main.rs` 中将工厂实例传入 `init_from_config` 的工厂列表
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
