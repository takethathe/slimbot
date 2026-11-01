# Utility Functions

对应模块：`src/utils/mod.rs`

## 概述

提供全局通用的辅助函数和常量。

## 文本截断

```rust
pub fn truncate_text_head_tail(text: &str, max_chars: usize) -> String
```

按字符数截断文本，保留头部和尾部，中间用省略号代替：

- `max_chars` 是字符数（非字节数），正确处理 UTF-8 多字节字符
- 尾部分为 `min(2000, max_chars / 2)` 字符
- 头部为剩余字符数
- 输出格式：`{head}\n... (truncated, N chars omitted) ...\n{tail}`

## 原子写入

```rust
pub fn write_file_atomic(path: &Path, content: &str) -> std::io::Result<()>
```

通过临时文件 + rename 实现原子写入：
1. 写入 `.filename.tmp`
2. `rename` 到目标路径（原子操作）
3. 自动创建父级目录

用于工具结果等需要避免部分写入的场景。

## 持久化引用

```rust
pub fn build_persisted_reference(
    file_path: &Path,
    content: &str,
    preview_max: usize,
) -> String
```

构建超长工具结果的引用字符串（`preview_max` 由 `pressure.persisted_preview_ratio` 派生，不再是固定常量），格式：

```
[tool output persisted]
Full output saved to: {path}
Original size: {n} chars
Preview:
{前 preview_max 字符}
...
(Read the saved file if you need the full output.)
```

## 常量

| 常量 | 值 | 说明 |
|------|-----|------|
| `TOOL_RESULTS_DIR` | `"tool-results"` | 工具结果存储目录名 |
| `HEAD_TAIL_CHUNK` | `2000` | 头尾截断的单块大小 |

## 工具结果文件

```rust
pub fn tool_result_rel_path(tool_call_id: &str) -> String          // "tool-results/<id>.txt"
pub fn existing_tool_result_path(workspace: &Path, id: &str) -> Option<String>
pub fn persist_tool_result_file(workspace: &Path, id: &str, content: &str) -> Option<String>
pub fn sweep_tool_results(workspace: &Path, ttl: Duration) -> usize
```

`persist_tool_result_file` 从不覆盖已有文件（首个完整副本获胜）；`sweep_tool_results` 按 mtime 删除过期文件，由 Dream 成功收尾调用（TTL = `pressure.tool_results_ttl_days`），不新建定时器。
