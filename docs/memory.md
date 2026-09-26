# Memory

对应模块：`src/memory/`

## 概述

`MemoryService` 是统一的记忆 facade，管理 SlimBot 的长期记忆（按 category 寻址的文档）与交互历史。它把两件事组合在一起：

- **category 文档操作** 经可插拔的 `MemoryBackend` trait（当前仅 markdown backend）
- **历史操作**（`history.jsonl` + Dream 游标）由独立的 `HistoryStore` 承担

## Memory category

记忆按语义 category 寻址，每个 category 对应一个文档：

| Category | 路径 | 内容 |
|----------|------|------|
| `Soul` | `{workspace}/SOUL.md` | Agent 行为规则、护栏、交互模式、工具使用策略 |
| `User` | `{workspace}/USER.md` | 用户属性：身份、偏好、习惯、沟通风格（语言、长度、语气） |
| `Agent` | `{workspace}/AGENTS.md` | Agent 工作指令 |
| `Tools` | `{workspace}/TOOLS.md` | 工具使用指南 |
| `Core` | `{workspace}/memory/MEMORY.md` | 长期事实：项目上下文、架构、战略决策 |
| `Daily` | `{workspace}/memory/YYYY-MM-DD.md` | 每日日志（预留，暂无写入方） |
| `Skill` | `{workspace}/skills/<name>/SKILL.md` | 可复用工作流模板（`name` 必填） |
| `Custom` | `{workspace}/memory/<name>.md` | 用户自定义 category（`name` 必填） |

`MemoryCategory` 是封闭的单元变体枚举，**不携带动态名字**：`Skill` / `Custom` 的名字在调用时通过 `name: Option<&str>` 单独传入，校验规则 `[a-z0-9-]{1,64}`。

## 寻址

所有文档级操作都形如 `(category, name)`：

```rust
let content = service.get(&MemoryCategory::Core, None).await?;
service.set(&MemoryCategory::Skill, Some("grep-workflow"), body).await?;
```

- 固定 category 忽略 `name`（作为 tag 元数据保留，为将来 namespace 过滤预留）
- `Skill` / `Custom` 必须提供合法名字

## MemoryBackend trait

```rust
#[async_trait]
pub trait MemoryBackend: Send + Sync {
    fn name(&self) -> &str;
    fn kind(&self) -> MemoryBackendKind;
    async fn get(&self, category: &MemoryCategory, name: Option<&str>) -> Result<Option<String>>;
    async fn set(&self, category: &MemoryCategory, name: Option<&str>, content: &str) -> Result<()>;
    async fn edit(&self, category: &MemoryCategory, name: Option<&str>, old: &str, new: &str) -> Result<EditOutcome>;
    async fn remove(&self, category: &MemoryCategory, name: Option<&str>) -> Result<bool>;
    async fn list_skill_names(&self) -> Result<Vec<String>>;
    async fn list_custom_names(&self) -> Result<Vec<String>>;
    fn template(&self, category: &MemoryCategory) -> Option<String>;
    async fn health_check(&self) -> bool { true }
    async fn sync(&self) -> Result<()> { Ok(()) }
}
```

- **文档级**（而非条目级）API：一次读写整个文档，匹配 SlimBot 的实际消费方式 —— 整文件注入 system prompt，加上 Dream 的外科手术式编辑
- `edit` 返回 `EditOutcome`：`Replaced` / `NotFound` / `Ambiguous(n)`（`old` 必须唯一匹配）
- 当前唯一实现是 `MarkdownBackend`（即上表文件布局）；`MemoryBackendKind::from_str_opt` 负责解析配置字符串，未知值报错

## MemoryService facade

```rust
pub struct MemoryService {
    memory_dir: PathBuf,
    backend: Arc<dyn MemoryBackend>,
    history: HistoryStore,
}

pub type SharedMemory = Arc<tokio::sync::Mutex<MemoryService>>;
```

| 方法 | 说明 |
|------|------|
| `new(workspace_dir)` | 使用默认 markdown backend 构建 |
| `with_backend(workspace_dir, backend)` | 使用显式 backend（配置驱动） |
| `init()` | 创建 `memory/` 目录 |
| `get` / `set` / `edit` / `remove` | 文档操作，委托 backend |
| `list_skill_names` / `list_custom_names` | 枚举动态名字 |
| `get_effective(category, name)` | 注入用有效内容：缺失 / 空 / 等于模板 → `None` |
| `sync_all()` | 落盘：`backend.sync()`（文档 fsync）+ `history.sync()`（`history.jsonl` 与游标 fsync） |
| `append_history` / `read_*` / 游标 / `compact_history_after_dream` | 历史操作，委托 `HistoryStore` |

## HistoryStore（历史子系统）

历史刻意**不**放进 `MemoryBackend` trait：它是「追加日志 + 游标」语义，与文档级 category API 形态不同，因此作为 memory 模块内的独立子系统。

| 文件 | 说明 |
|------|------|
| `history.jsonl` | 交互历史（追加写入，每条含 `timestamp`、`content`） |
| `.dream_cursor` | Dream 处理游标 |

| 方法 | 说明 |
|------|------|
| `append_history(entry)` | 追加一条记录 |
| `read_entries()` / `read_unprocessed_history(skip)` / `read_recent_history(n)` | 读取 |
| `get_last_dream_cursor()` / `set_last_dream_cursor(cursor)` | Dream 游标 |
| `compact_history_after_dream()` | 超限时压缩（保留 `DREAM_KEEP_MINIMUM`） |

游标文件单独存储，避免每次追加历史都读全文件。

## 配置

```jsonc
{
  "memory": { "backend": "markdown" }
}
```

`memory.backend` 由 config schema 校验，当前白名单仅 `"markdown"`，非法值回退默认。字段预留以便未来接入 sqlite 等 backend。

## Dream 工具

Dream 通过 memory 工具（而非文件工具）维护记忆。`category` 为白名单 `soul` / `user` / `core` / `skill`，`name` 仅在 `category=skill` 时必填：

| 工具 | 说明 |
|------|------|
| `memory_read(category, name?)` | 读取一个文档 |
| `memory_write(category, name?, content)` | 创建或覆盖文档；允许创建新 skill |
| `memory_edit(category, name?, old_string, new_string)` | 唯一匹配的原地替换 |

已存在的 skill 名列表注入每个工具的 description。Dream 沙箱**没有**任何文件系统工具。

## 设计考量

- 历史记录使用**追加写入**（与 session JSONL 的全量写入不同）
- `get_effective` 承接「未修改模板不注入」语义（替代旧的 `read_if_modified`）
