# I/O Scheduler

对应模块：`src/io_scheduler.rs`

## 概述

`IoScheduler` 协调通道的 I/O 执行，将阻塞读取（如 stdin）路由到 `spawn_blocking` 执行器，避免阻塞 tokio 运行时。

## 结构

```rust
pub struct IoScheduler {
    inbound_tx: mpsc::Sender<BusRequest>,
}
```

## `submit_blocking_read_loop`

```rust
pub fn submit_blocking_read_loop(
    &self,
    session_id: String,
    hook: TaskHook,
    channel_name: String,
    prompt: String,
) -> JoinHandle<()>
```

启动一个持续的 stdin 读取循环：

```
tokio::spawn:
  loop:
    spawn_blocking:
      print(prompt)
      flush(stdout)
      read_line(stdin)
    match result:
      Ok(input)     → 构建 BusRequest → inbound_tx.send()
      Eof           → 退出循环
      Empty         → 跳过
      Other error   → warn! → 继续
      Panic         → error! → 退出
```

## IoReadError

阻塞读取的错误类型：

| 变体 | 说明 |
|------|------|
| `Eof` | 输入流结束（stdin 关闭） |
| `Empty` | 空白输入，应跳过 |
| `Other` | 其他 I/O 错误 |

## IoHandle

```rust
pub struct IoHandle {
    pub join_handle: Option<JoinHandle<()>>,
    pub session_id: String,
    pub channel_name: String,
}
```

通道启动 I/O 循环时返回的句柄，供 `ChannelManager` 进行生命周期管理。
