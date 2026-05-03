# Session Manager

对应模块：`src/session.rs`

## 概述

`SessionManager` 管理所有会话的生命周期，包括消息存储、任务队列和 JSONL 持久化。

## 核心类型

### Message

```rust
pub enum Message {
    System { meta: MessageMeta { id: usize }, content: String },
    User { meta: MessageMeta { id: usize }, content: String },
    Assistant { meta: MessageMeta { id: usize }, content: Option<String>, tool_calls: Option<Vec<ToolCall>> },
    Tool { meta: MessageMeta { id: usize }, content: String, tool_call_id: String, name: Option<String> },
}
```

每条消息有唯一的自增 `id`，用于 consolidation 游标跟踪。

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
    pub messages: Vec<Message>,             // 消息列表（已驱逐的消息已物理移除）
    meta: SessionMeta,                      // 会话元数据
    last_persisted_idx: usize,              // 已持久化消息的索引偏移
}
```

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
| `add_message(session_id, msg)` | 向 Session 追加消息（自动分配 ID） |
| `get_messages(session_id)` | 获取 Session 的未驱逐消息列表（consolidation 后的消息已从内存移除） |
| `get_session_data(session_id)` | 获取会话数据快照（含 token ratio 和 consolidation cursor） |
| `get_last_summary(session_id)` | 获取最后一次 consolidate 的摘要 |
| `set_last_summary(session_id, summary)` | 设置 consolidate 摘要 |
| `update_consolidation_cursor(session_id, cursor_id)` | 更新游标，物理移除 id <= cursor_id 的消息 |
| `persist(session_id)` | **追加**写入 JSONL 文件（增量写入） + 原子更新 meta |
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

- **追加写入**：`persist()` 只写入 `last_persisted_idx` 之后的新消息到 JSONL
- **Consolidation 重置**：驱逐消息后，`last_persisted_idx` 设为剩余消息数（已全在磁盘上）
- **Meta 文件**：每次 `persist()` 和 `update_consolidation_cursor()` 时原子更新

### 加载

`get_or_create()` 从 JSONL 文件加载消息时：
- 跳过 `id <= last_consolidated_id` 的消息（已驱逐）
- 向后兼容：旧格式消息无 ID 字段（serde 默认为 0），当 `last_consolidated_id=0` 时全部加载

## Consolidation 游标

`update_consolidation_cursor(session_id, cursor_id)` 执行以下操作：
1. 设置 `last_consolidated_id = cursor_id`
2. 从内存中物理移除所有 `id <= cursor_id` 的消息（`retain` 过滤）
3. 将 `last_persisted_idx` 设为剩余消息数（已全在磁盘上，下次 persist 不会重复写入）
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
