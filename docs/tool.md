# Tool System

对应模块：`src/tool.rs` + `src/tools/`

## 概述

SlimBot 的工具系统通过 `Tool` trait 定义统一接口，`ToolManager` 管理工具注册、配置和 OpenAI function calling 格式转换。

## Tool Trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;
    async fn execute(&self, args: serde_json::Value) -> Result<String>;
}
```

实现此 trait 即可接入任意工具。

## ToolManager

```rust
pub struct ToolManager {
    tools: HashMap<String, Box<dyn Tool>>,
    workspace_dir: PathBuf,
}
```

### `init_from_config`

从配置的 `tools` 数组注册工具。如果未配置，默认启用所有内置工具：

| 工具 | 说明 |
|------|------|
| `shell` | Shell 命令执行 |
| `file_reader` | 文件读取 |
| `file_writer` | 文件写入 |
| `file_editor` | 搜索替换编辑 |
| `list_dir` | 目录列表 |
| `make_dir` | 目录创建 |

### `to_openai_functions`

返回所有已注册工具的定义，转换为 OpenAI function calling 格式：

```rust
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,  // JSON Schema
}
```

### `execute`

按名称查找并执行工具。

## 工具结果处理

### 空结果防护

```rust
pub fn ensure_nonempty_tool_result(tool_name: &str, content: &str) -> String
```

空字符串替换为 `(tool_name completed with no output)`，避免 LLM 收到空响应后困惑。

### 错误格式化

```rust
pub fn format_tool_error(error_msg: &str) -> String
```

格式化为 `Error: ...\n\n[Analyze the error above and try a different approach.]`，不中断 ReAct 循环。

### 超长结果持久化

```rust
pub fn persist_tool_result(
    workspace_dir: &Path,
    tool_call_id: &str,
    content: &str,
    max_chars: usize,
) -> String
```

当工具结果超过 `max_chars` 时：
1. 将完整内容写入 `{workspace}/tool-results/{tool_call_id}.txt`（原子写入）
2. 返回引用字符串 + 预览（前 1200 字符）
3. LLM 可通过引用路径读取完整内容

### 头尾截断

通过 `utils::truncate_text_head_tail` 截断超长文本，保留头部和尾部各 2000 字符，中间用省略号代替。

## 内置工具

详见 [内置工具](tools.md)。
