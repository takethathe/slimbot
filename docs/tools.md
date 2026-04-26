# 内置工具

SlimBot 内置 6 个工具，用于 Shell 执行和文件操作。所有工具默认启用，可在 `config.json` 中单独禁用。

## 工具列表

### `shell`

通过 `sh -c` 执行任意 Shell 命令。

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `command` | string | 是 | 要执行的 Shell 命令 |

**行为：**
- 返回 stdout 和 stderr（分别带有 `stdout:` / `stderr:` 前缀）
- 默认超时时间：30 秒。超过此时间将返回错误。
- 退出码非零时会附加到输出末尾。

**示例：**
```json
{ "command": "ls -la /tmp" }
```

---

### `file_reader`

读取数据目录内的文件内容。

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `path` | string | 是 | 文件路径（相对于数据目录） |

**行为：**
- 输出被截断到 50,000 字符，防止上下文爆炸。
- 如果路径不是文件或不存，返回错误。

**示例：**
```json
{ "path": "workspace/agent.md" }
```

---

### `file_writer`

将内容写入文件，自动创建文件及父目录。

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `path` | string | 是 | 文件路径（相对于数据目录） |
| `content` | string | 是 | 要写入的内容 |

**行为：**
- 如果文件不存在则创建，已存在则覆盖。
- 自动创建父目录。

**示例：**
```json
{ "path": "workspace/notes.md", "content": "# 笔记\n\n这里是内容。" }
```

---

### `file_editor`

在文件中执行精确的搜索和替换。

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `path` | string | 是 | 文件路径（相对于数据目录） |
| `old_string` | string | 是 | 要查找的精确文本（必须**恰好出现一次**） |
| `new_string` | string | 是 | 替换后的文本 |

**行为：**
- 如果未找到 `old_string` 或出现多次，拒绝该编辑。
- 这确保了精确操作——不会意外批量替换。

**示例：**
```json
{
  "path": "workspace/agent.md",
  "old_string": "你是一个有帮助的助手。",
  "new_string": "你是一个简洁的助手。"
}
```

---

### `list_dir`

列出数据目录内的目录内容。

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `path` | string | 是 | 目录路径（相对于数据目录） |

**行为：**
- 条目按字母顺序排序。
- 每个条目前带有类型标识符：
  - `[D]` — 目录
  - `[F]` — 文件
  - `[L]` — 符号链接或其他

**示例：**
```json
{ "path": "workspace" }
```

---

### `make_dir`

创建目录，包括所有父目录。

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `path` | string | 是 | 目录路径（相对于数据目录） |

**行为：**
- 等价于 `mkdir -p`。目录已存在时不报错。

**示例：**
```json
{ "path": "workspace/skills/new-skill" }
```

## 安全

所有文件操作仅限于 `config.json` 中配置的 `workspace_dir` 范围内。

- **路径验证**：每个文件路径都相对于 `workspace_dir` 解析，并使用规范绝对路径进行验证。试图通过 `../`、符号链接等方式逃逸工作目录的路径会被拒绝。
- **Shell 隔离**：`shell` 工具不受目录限制，但受 30 秒超时约束。
- **读取大小限制**：`file_reader` 输出上限为 50,000 字符。

## 配置

工具在 `config.json` 的 `tools` 数组中配置。如果该数组为空，所有 6 个内置工具默认启用。

```json
{
  "tools": [
    { "name": "shell", "enabled": true },
    { "name": "file_reader", "enabled": true },
    { "name": "file_writer", "enabled": true },
    { "name": "file_editor", "enabled": true },
    { "name": "list_dir", "enabled": false },
    { "name": "make_dir", "enabled": true }
  ]
}
```

要禁用某个工具，将其 `enabled` 字段设为 `false`，或直接移除该条目。

## 添加自定义工具

新工具可以通过实现 `src/tool.rs` 中的 `Tool` trait 来添加：

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;
    async fn execute(&self, args: serde_json::Value) -> Result<String>;
}
```

1. 在 `src/tools/` 下创建新模块定义工具结构体。
2. 在 `src/tools/mod.rs` 的 `create_tool()` 工厂函数中注册。
3. 可选在 `ToolManager::init_from_config()` 的默认列表中添加该工具名称。

`parameters()` 方法返回一个 JSON Schema 对象，描述工具的参数，随后会被转换为 OpenAI 函数调用格式。
