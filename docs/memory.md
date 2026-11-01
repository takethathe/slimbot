# Memory

对应模块：`src/memory/`

## 概述

`MemoryService` 是统一的记忆 facade，管理 SlimBot 的长期记忆（按 category 寻址的文档）与交互历史。它把两件事组合在一起：

- **category 文档操作** 经可插拔的 `MemoryBackend` trait（当前仅 markdown backend）
- **历史操作**（`history.jsonl` + Dream 游标）由独立的 `HistoryStore` 承担

## 写侧：单文件漏斗（ADR 0012）

主循环**没有**记忆写工具：`src/tools/mod.rs::create_tool` 里没有、`AgentLoop` 装配时也不注册（由 `test_tool_factory_registers_no_memory_write_tool` 与 `test_agent_visible_tools_are_read_only_for_memory` 守住）。agent 落记忆的方式只有一件事 —— 用既有的 `file_writer` / `file_editor` 改 `{workspace}/memory/MEMORY.md`。

```
对话现场（主模型）      Dream（唯一分类者）
MEMORY.md  ──────────▶  SOUL / USER / AGENTS / TOOLS / skills/<name>/SKILL.md / memory/<name>.md
（即时、只用 file 工具）    （整理、合并、晋升）
```

- **可写集只有一篇文档**：`memory/MEMORY.md`（Core）。分类（这条事实是 soul、user 还是操作细节）需要全量上下文，交给 Dream 比交给正在对话的模型可靠；工具数量本身是幻觉预算。
- **保护路径由代码强制**（不是提示词）：`bootstrap::protected_documents()`（AGENTS/USER/SOUL/TOOLS + `agents/trigger.md`，从 bootstrap 列表派生、去掉 MEMORY.md）加上 `skills/*/SKILL.md`、`memory/*.md`（MEMORY.md 除外），命中即由 `src/tools/protected.rs` 返回 `REFUSED:` 文案，**不退回合、不报错**。运行文件（`cron/jobs.json`、`sessions/*.jsonl`、`HEARTBEAT.md`、`memory/history.jsonl`）照常可写 —— 把功能阉割当安全不是保护。细节见 [Tool System](tool.md) 的「受保护文档」一节。
- **provenance 是写法约定，不是字段**：模板与 memory skill 要求每条 bullet 自带 `(source · YYYY-MM-DD)`。派生索引**不解析**它、代码也**不做门禁**（`test_memory_skill_documents_the_provenance_convention` 锁的是这段提示词文本）。给来源赋予机器语义就等于承诺一个守不住的校验：写入路径是普通文本编辑。
- **原文不入记忆**：外部内容进上下文的唯一入口是有界 evidence（ADR 0009，`text ≤ 200` 字符 / ≤ 5 条），原文留在引用路径（`tool-results/<id>.txt`），`MEMORY.md` 只收消化后的表述 + 来源。**残余风险**：`web_fetch` 与 owner 对话没有上限，这两条路径上「只写消化表述」靠提示词与 Dream 事后审查，代码不拦（同理 `shell` 可以直接改任何文件）。
- **`Daily` 不建设**：无写者、不进检索语料；`history.jsonl` 同样不进语料 —— 未经整理的一轮对话不是事实。由 `test_search_corpus_excludes_daily_documents_and_history` 守住。

## Memory category

记忆按语义 category 寻址，每个 category 对应一个文档：

| Category | 路径 | 内容 |
|----------|------|------|
| `Soul` | `{workspace}/SOUL.md` | Agent 行为规则、护栏、交互模式、工具使用策略 |
| `User` | `{workspace}/USER.md` | 用户属性：身份、偏好、习惯、沟通风格（语言、长度、语气） |
| `Agent` | `{workspace}/AGENTS.md` | Agent 工作指令 |
| `Tools` | `{workspace}/TOOLS.md` | 工具使用指南 |
| `Core` | `{workspace}/memory/MEMORY.md` | 长期事实：项目上下文、架构、战略决策 |
| `Daily` | `{workspace}/memory/YYYY-MM-DD.md` | 每日日志——**v1 不建设**：无写者、不进检索语料（ADR 0012） |
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

Dream 通过 memory 工具（而非文件工具）维护记忆。这套注册**只属于 Dream 沙箱**（`DreamService::build_tools`），不进 agent 的工具表：主模型只通过 `file_editor` 改 `MEMORY.md`，不存在第二个写入入口。`category` 为白名单 `soul` / `user` / `core` / `skill`，`name` 仅在 `category=skill` 时必填：

| 工具 | 说明 |
|------|------|
| `memory_read(category, name?)` | 读取一个文档 |
| `memory_write(category, name?, content)` | 创建或覆盖文档；允许创建新 skill |
| `memory_edit(category, name?, old_string, new_string)` | 唯一匹配的原地替换 |

已存在的 skill 名列表注入每个工具的 description。Dream 沙箱**没有**任何文件系统工具。字段与行为细节见 [内置工具](tools.md) 的「Dream 专用工具」一节。

## Memory entry 与 Memory index（派生检索层）

记忆文档是唯一**权威**；`MemoryEntry` 与 `MemoryIndex` 是从文档**派生**的可检索视图，**永不落盘**、可随时重建。这**有意偏离** zeroclaw 的「entry 权威」模型，理由见 [ADR 0004](adr/0004-derived-entry-index-layer.md)；ADR 0003 的文档级 API 继续有效，两者是叠加关系。

**Memory entry**（`src/memory/index.rs` 的 `MemoryEntry`）是记忆文档中可独立检索的最小单元，字段：

| 字段 | 说明 |
|------|------|
| `content` | 原文段落，返回给调用方 |
| `category` | 来源 category |
| `name` | `Skill` / `Custom` 的动态名 |
| `source` | 相对 workspace 的文档路径，如 `memory/MEMORY.md` |
| `heading` | 最近的 ATX 标题文本（`#`–`######`） |
| `hash` | 规范化内容哈希（进程内身份、将来 dedup 的原料；不可持久化） |

**解析边界**（`parse_document`）：

- entry 边界 = **空行分隔的段落块**（连续非空行归为一条）
- ATX 标题行只更新当前 `heading`，自身不产出 entry
- `Skill` 文档开头的 YAML frontmatter 被剥除（元数据已由 available-skills 列表注入）；复用 `src/memory/frontmatter.rs` 的解析器
- 围栏代码块（``` / `~~~`）内不按空行切分，整块作为一条 entry；围栏标记行本身不索引
- 空文档 / 缺失文档 / 仅含标题的文档产出 0 条 entry，不报错

**Memory index**（`MemoryIndex` trait + `KeywordIndex` 实现）纯内存、惰性构建：

- 首次检索时构建，之后每次检索先比对**内容指纹**（可枚举文档 `category`/`name`/`source`/`content` 的哈希）——不一致才重新解析。取内容而非 mtime/size，因为文档级 backend 不暴露路径与 stat，内容哈希零 trait 改动且对未来 sqlite backend 同样成立。
- 不做后台文件监听，不落盘索引文件，因此没有「索引陈旧」类 bug 与需要迁移的索引文件。
- 可枚举文档 = `MemoryCategory::ALL` 去掉 `Daily` / `Skill` / `Custom` 的 5 个固定文档，加 `list_skill_names()` 与 `list_custom_names()`。全部经 `MemoryBackend` 读取。
- `MemoryIndex` trait 是预留的接缝：真正的倒排索引 / FTS5 / 向量库可替换 `KeywordIndex` 而不动调用方。

## 分词与打分

- **拉丁**：小写化，按非字母数字边界切分，丢弃长度 < 2 的 token。
- **CJK**：按**相邻字对（bigram）**索引；单字查询回退到 unigram 子串匹配。
- **打分**：BM25-lite（idf + tf 饱和，k1 = 1.2，b = 0.75）。命中 `heading` 的 token 权重为正文的 2 倍。
- **查询语义**：OR（命中任一 token 即候选，至少命中一个才计入结果）；不支持短语 / 前缀 / 模糊。
- **确定性 tie-break**：分数降序 → category 规范顺序（`MemoryCategory::ALL`）→ `source` → `heading` → `hash`。

中文检索是**不复用** FTS5 默认 tokenizer 的直接原因：FTS5 的 `unicode61` 对中文不分词（需 `trigram`），照抄 zeroclaw 的 Bm25 路径并不能解决中文检索。

## memory_search（主循环只读工具）

主循环在 CLI 与 gateway 两种模式下都注册 `memory_search`（`AgentLoop::from_parts`；与 `message` / `cron` 一样**不**进 `config.tools`，因为后者只能构造仅需 workspace 的内置工具）。

`category` 参数只是结果过滤器，**不是**权限边界（主循环本就持有 `file_reader`）。返回值是纯文本（汇总行 + 逐条 category / heading / source），不引入结构化协议；无匹配时返回明确文案而非空串。参数、打分与检索范围的完整说明见 [内置工具](tools.md) 的 `memory_search` 一节。

## 设计考量

- 历史记录使用**追加写入**（与 session JSONL 的全量写入不同）
- `get_effective` 承接「未修改模板不注入」语义（替代旧的 `read_if_modified`）
- 检索是「按需查询」，**不**替换整文档注入——system prompt 布局完全不变，prompt-cache 行为不受扰动
