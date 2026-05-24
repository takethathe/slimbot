# Session Manager

对应模块：`src/session.rs`

## 概述

`SessionManager` 管理所有会话的生命周期，包括消息存储、任务队列和 JSONL 持久化。

## 核心类型

### Message

```rust
pub enum Message {
    System { meta: MessageMeta, content: Content },
    User { meta: MessageMeta, content: Content, runtime_content: Option<String> },
    Assistant { meta: MessageMeta, content: Option<Content>, tool_calls: Option<Vec<ToolCall>> },
    Tool { meta: MessageMeta, content: Content, tool_call_id: String, name: Option<String> },
}
```

每条消息有唯一的自增 `id`，用于 consolidation 游标跟踪。

`Message::User` 的 `runtime_content` 字段存储运行时上下文（如当前时间、channel 信息、会话摘要等），仅在 Provider 序列化时处理，**永远不会写入 JSONL**（通过 `skip_serializing_if` 保证）。

**便利构造函数**（避免手动构造 `meta`）：
```rust
Message::user("hello".to_string())
Message::assistant(Some("hi".to_string()), None)
Message::assistant(Some("reply".to_string()), Some(vec![tool_call]))
Message::system("instructions".to_string())
Message::tool("result".to_string(), "call-id".to_string(), Some("tool_name"))
```

### SessionMeta

```rust
pub struct SessionMeta {
    pub last_consolidated_id: usize,           // 已摘要消息的最大 ID
    pub next_message_id: usize,                 // 下一个自增消息 ID
    pub char_per_token_ratio: f64,              // 平均每 token 字符数（默认 4.0）
    pub last_summary: Option<String>,           // 最后一次 consolidate 的摘要
}
```

`SessionMeta` 持久化为 `{session_id}.meta.json`，与消息 JSONL 文件分离。使用 `write_file_atomic` 保证写入安全。

### SessionData

```rust
pub struct SessionData {
    pub messages: Vec<Message>,
    pub char_per_token_ratio: f64,
    pub last_consolidated_id: usize,
}
```

用于 consolidation 分析的会话数据快照。

### SessionTask

```rust
pub struct SessionTask {
    pub id: String,              // 任务唯一 ID（UUID）
    pub session_id: String,      // 所属会话 ID
    pub content: String,         // 任务内容（用户输入）
    pub hook: TaskHook,          // 状态通知钩子
    pub state: TaskState,        // 当前状态
    pub closure: Option<Box<dyn FnOnce() -> BoxFuture + Send>>, // 执行闭包
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
    event_tx: Option<tokio::sync::broadcast::Sender<AgentEvent>>,       // AgentEvent 广播通道
    session_id: String,                                       // 所属 Session ID
}
```

`TaskHook` 通过构建器模式组合：
```rust
TaskHook::new(&session_id)
    .with_status_channel(status_tx)   // 绑定状态通知通道
    .with_events(event_tx)            // 绑定 AgentEvent 广播通道
```

### AgentEvent

Agent 运行过程中通过 `TaskHook.fire_event()` 发出的实时事件，使用 `#[serde(tag = "type")]` 序列化供前端消费：

```rust
pub enum AgentEvent {
    TaskStarted { session_id: String },
    TaskCompleted { session_id: String, result: String },
    TaskFailed { session_id: String, error: String },
    PreIteration { session_id: String, iteration: u32 },
    PostIteration { session_id: String, iteration: u32 },
    AssistantMessage { session_id: String, content: String },
    ToolCall { session_id: String, name: String, args: String },
    ToolResult { session_id: String, name: String, output: String },
}
```

每个 variant 提供 `session_id()` 方法用于事件路由。

### Session

```rust
pub struct Session {
    pub id: String,                         // 会话 ID
    pub history: Arc<[Message]>,            // 已持久化的干净消息（不可变，零拷贝共享）
    pub current_turn: Vec<Message>,         // 本轮消息缓冲区（可能包含 runtime_content）
    meta: SessionMeta,                      // 会话元数据
}
```

`Session` 采用双列表结构：`history` 使用 `Arc<[Message]>` 实现零拷贝共享已持久化的历史消息，`current_turn` 是本轮新增消息的独立缓冲区。`persist()` 时将 `current_turn` 中的消息 strip `runtime_content` 后追加到 JSONL，然后合并进 `history`。

`Session` 的内部方法：
- `last_consolidated_id()` — 返回已合并消息的最大 ID
- `next_message_id()` — 返回下一个自增 ID
- `char_per_token_ratio()` — 返回字符/token 比率
- `last_summary()` — 返回上次合并摘要
- `set_last_summary(summary)` — 设置摘要（空值或 `"(nothing)"` 时设为 `None`）
- `update_token_ratio(prompt_tokens)` — 根据当前消息总字符数和 prompt tokens 更新比率
- `update_consolidated_id(id)` — 内部方法，更新游标并物理移除旧消息

### SessionManager

```rust
pub struct SessionManager {
    sessions: HashMap<String, Session>,    // 所有活跃 Session
    runners: HashMap<String, SessionRunner>,  // 每会话执行协调器
    session_dir: PathBuf,                  // JSONL 存储目录
}
```

### SharedSessionManager

```rust
pub type SharedSessionManager = Arc<Mutex<SessionManager>>;
```

### SessionRunner

```rust
pub struct SessionRunner {
    running: Arc<AtomicBool>,              // 执行标志
    task_queue: Arc<Mutex<VecDeque<...>>>, // 任务队列
}
```

每个 session 独立的执行协调器，保证顺序执行，空闲时自动拉取下一个任务。

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
| `get_or_create(session_id)` | 获取已有 Session，或从 JSONL/Meta 加载并创建 |
| `add_message(session_id, msg)` | 向 Session 追加消息到 `current_turn`（自动分配 ID） |
| `get_messages(session_id)` | 获取 Session 的完整消息列表（`history` + `current_turn`） |
| `get_session_data(session_id)` | 获取会话数据快照（含 token ratio 和 consolidation cursor） |
| `get_history_arc(session_id)` | 获取 `Arc<[Message]>` 引用的历史消息（O(1) clone） |
| `get_current_turn_messages(session_id)` | 获取本轮新增消息的 clone |
| `message_count(session_id)` | 获取 `current_turn` 中的消息数，用于 runner 回滚计数 |
| `total_message_count(session_id)` | 获取总消息数（`history` + `current_turn`），用于展示 |
| `get_last_summary(session_id)` | 获取最后一次 consolidate 的摘要 |
| `set_last_summary(session_id, summary)` | 设置 consolidate 摘要 |
| `update_consolidation_cursor(session_id, cursor_id)` | 更新游标，物理移除 id <= cursor_id 的消息 |
| `persist(session_id)` | **追加**写入 JSONL + 保存 meta（即使无新消息也会保存 meta） |
| `truncate_messages(session_id, count)` | 截断 `current_turn` 到指定数量，用于回滚 |
| `update_token_ratio(session_id, prompt_tokens)` | 更新字符/token 比率 |
| `submit_task(task)` | 提交 SessionTask，保证顺序执行 |

## 消息 ID 机制

每条消息在添加到 session 时自动分配唯一的自增 ID：

- 新 session 从 1 开始
- 从磁盘加载时从 `next_message_id`（meta 文件中保存的值）继续
- 若文件中存在更大的 ID，自动上调 `next_message_id`
- ID 用于 consolidation 游标，区分已驱逐和未驱逐的消息

## 持久化

### 文件布局

| 文件 | 说明 |
|------|------|
| `{session_id}.jsonl` | 消息历史，每行一条 JSON 序列化的 Message |
| `{session_id}.meta.json` | 会话元数据（consolidation cursor、next_message_id、token ratio、summary） |

### 写入格式

JSONL 文件中每条消息携带 `id` 字段（通过 serde flatten 嵌入）：

```json
{"role":"user","id":1,"content":"你好"}
{"role":"assistant","id":2,"content":"你好！","tool_calls":null}
```

### 写入策略

- **追加写入**：`persist()` 将 `current_turn` 中的新消息追加到 JSONL，然后合并进 `history` 并清空 `current_turn`
- **Meta 文件**：每次 `persist()` 和 `update_consolidation_cursor()` 时原子更新。`persist()` 始终保存 meta，即使没有新消息（如 `set_last_summary` 后的 persist）

### 加载

`get_or_create()` 从 JSONL 文件加载消息时：
- 跳过 `id <= last_consolidated_id` 的消息（已驱逐）
- 向后兼容：旧格式消息无 ID 字段（serde 默认为 0），当 `last_consolidated_id=0` 时全部加载

## Consolidation 游标

`update_consolidation_cursor(session_id, cursor_id)` 执行以下操作：
1. 先将 `current_turn` 合并到 `history`（确保未提交消息不丢失）
2. 设置 `last_consolidated_id = cursor_id`
3. 从 `history` 中物理移除所有 `id <= cursor_id` 的消息（`filter` 过滤）
4. 持久化 meta 文件

重新加载时，JSONL 文件中 id <= cursor 的消息会被跳过。

## Token 估算

`message_content_chars(msg)` 计算消息的可见文本字符数，配合 `char_per_token_ratio` 用于估算 token 消耗：

```
estimated_tokens = total_chars / char_per_token_ratio
```

`char_per_token_ratio` 默认 `4.0`（4 字符/token），`Session::update_token_ratio()` 在每次 LLM 调用后根据实际 prompt tokens 更新该值。

## 并发模型

所有 Session 操作通过 `SharedSessionManager = Arc<Mutex<SessionManager>>` 保护：

- 多个通道和 Runner 可以同时持有 SharedSessionManager 的引用
- 每次操作前 `lock().await` 获取独占锁
- `SessionRunner` 使用 `AtomicBool` 和独立任务队列保证同一 session 的任务顺序执行
