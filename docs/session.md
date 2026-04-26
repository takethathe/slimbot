# Session Manager

对应模块：`src/session.rs`

## 概述

`SessionManager` 管理所有会话的生命周期，包括消息存储、任务队列和 JSONL 持久化。

## 核心类型

### Message

```rust
pub enum Message {
    System { content: String },           // 系统提示
    User { content: String },             // 用户消息
    Assistant { content: Option<String>, tool_calls: Option<Vec<ToolCall>> },  // 助手回复
    Tool { content: String, tool_call_id: String, name: Option<String> },  // 工具结果
}
```

Tool 消息的 `name` 字段记录工具名称，用于历史消息治理中的孤立检测。

### SessionTask

```rust
pub struct SessionTask {
    pub id: String,              // 任务唯一 ID（UUID）
    pub content: String,         // 任务内容（用户输入）
    pub hook: TaskHook,          // 状态通知钩子
    pub state: TaskState,        // 当前状态
}
```

### TaskState

```rust
pub enum TaskState {
    Pending,                                              // 等待执行
    Running { current_iteration: u32 },                   // 执行中
    Completed { result: String },                         // 已完成
    Failed { error: String },                             // 失败
}
```

### TaskHook

```rust
pub struct TaskHook {
    status_tx: Option<tokio::sync::mpsc::Sender<(String, TaskState)>>,  // 状态通道
    session_id: String,                                       // 所属 Session ID
}
```

`TaskHook` 通过构建器模式组合：
```rust
TaskHook::new(&session_id)
    .with_status_channel(status_tx)   // 绑定状态通知通道
```

### Session

```rust
pub struct Session {
    pub id: String,                         // 会话 ID
    pub task_queue: VecDeque<SessionTask>,  // FIFO 任务队列
    pub tasks: HashMap<String, TaskState>,  // 任务状态映射
    pub messages: Vec<Message>,             // 消息列表
}
```

### SessionManager

```rust
pub struct SessionManager {
    sessions: HashMap<String, Session>,    // 所有活跃 Session
    session_dir: PathBuf,                  // JSONL 存储目录
}
```

### SharedSessionManager

```rust
pub type SharedSessionManager = Arc<Mutex<SessionManager>>;
```

## Session ID 格式

```
{channel_id}:{chat_id}
```

例如：`cli:abc123` 表示 CLI 通道中 ID 为 `abc123` 的聊天。

## 公共方法

| 方法 | 说明 |
|------|------|
| `new(session_dir)` | 创建 SessionManager，自动创建目录 |
| `create_id()` | 生成 UUID 作为 Session ID |
| `get_or_create(session_id)` | 获取已有 Session，或从 JSONL 加载历史并创建 |
| `add_message(session_id, msg)` | 向 Session 追加消息 |
| `enqueue_task(session_id, task)` | 将任务加入 FIFO 队列 |
| `dequeue_task(session_id)` | 从队列取出任务 |
| `update_task_state(session_id, task_id, state)` | 更新任务状态 |
| `get_messages(session_id)` | 获取 Session 的完整消息列表 |
| `persist(session_id)` | **全量**写入 JSONL 文件 |

## JSONL 持久化

### 写入格式

每行一条 JSON 序列化的 Message：

```json
{"role":"system","content":"..."}
{"role":"user","content":"你好"}
{"role":"assistant","content":"你好！有什么可以帮助你的？"}
```

### 写入策略

- **全量写入**：每次 `persist()` 写入 Session 的所有消息（非追加）
- **写入时机**：AgentRunner 循环结束后（完成或失败时都会触发）

### 加载

`SessionManager::load_messages_from_jsonl()` 从 JSONL 文件逐行反序列化，恢复消息历史。文件不存在时返回空列表。

## 并发模型

所有 Session 操作通过 `SharedSessionManager = Arc<Mutex<SessionManager>>` 保护：

- 多个通道可以同时持有 SharedSessionManager 的引用
- 每次操作前 `lock().await` 获取独占锁
- 锁的持有时间应尽可能短，避免阻塞其他通道
