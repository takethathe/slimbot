# Memory Store

对应模块：`src/memory.rs`

## 概述

`MemoryStore` 管理 SlimBot 的长期记忆和历史记录，支持 Agent 跨会话学习和自我进化。

## 文件布局

所有文件位于 `{workspace}/memory/` 目录：

| 文件 | 说明 |
|------|------|
| `MEMORY.md` | 长期记忆（Agent 学习到的用户偏好、行为模式等） |
| `history.jsonl` | 交互历史（带游标的追加日志） |
| `.cursor` | 当前处理游标（自增整数） |
| `.dream_cursor` | Dream 模块的处理游标 |

## 结构

```rust
pub struct MemoryStore {
    workspace_dir: PathBuf,
    memory_dir: PathBuf,
    memory_file: PathBuf,
    history_file: PathBuf,
    cursor_file: PathBuf,
    dream_cursor_file: PathBuf,
}
```

## 长期记忆

| 方法 | 说明 |
|------|------|
| `read_memory()` | 读取 `MEMORY.md` 内容 |
| `write_memory(content)` | 写入 `MEMORY.md` |
| `get_memory_context()` | 格式化为 system prompt 注入内容 |

## SOUL.md / USER.md

| 方法 | 说明 |
|------|------|
| `read_soul()` / `write_soul(content)` | 读取/写入 workspace 根目录的 `SOUL.md` |
| `read_user()` / `write_user(content)` | 读取/写入 workspace 根目录的 `USER.md` |

## 历史管理

### 写入

```rust
pub fn append_history(&self, entry: &str) -> Result<u64>
```

- 每条记录包含 `cursor`（自增）、`timestamp`、`content`
- 先写游标文件，再追加到 JSONL 文件（防止崩溃后游标重复）

### 读取

| 方法 | 说明 |
|------|------|
| `read_unprocessed_history(since_cursor)` | 读取游标之后的未处理记录 |
| `read_recent_history(max_entries)` | 读取最近 N 条记录 |

### 游标管理

| 方法 | 说明 |
|------|------|
| `next_cursor()` | 返回下一个自增游标值 |
| `get_last_dream_cursor()` | 读取 Dream 处理游标 |
| `set_last_dream_cursor(cursor)` | 更新 Dream 处理游标 |

## 设计考量

- 历史记录使用**追加写入**（与 session JSONL 的全量写入不同）
- 游标文件单独存储，避免每次写历史都读全文件
- `read_last_entry` 使用**反向扫描**策略：从文件末尾逐步扩大窗口读取，避免全文件加载
