# Bootstrap & Embedded Files

对应模块：`src/bootstrap.rs` + `src/embed.rs`

## 概述

`bootstrap.rs` 提供嵌入模板文件的访问，`embed.rs` 通过 `include!` 引入构建时生成的嵌入文件代码。

## Bootstrap 模板

### Bootstrap 文件

`setup` 时自动创建到 workspace 的 8 个文件（若不存在）：

| 文件 | 写入位置 | 说明 |
|------|----------|------|
| `AGENTS.md` | workspace 根 | Agent 工作指令 |
| `USER.md` | workspace 根 | 用户画像 |
| `SOUL.md` | workspace 根 | Agent 人格 |
| `TOOLS.md` | workspace 根 | 工具使用指南 |
| `MEMORY.md` | `memory/` | 长期记忆（初始为模板，未修改时不注入） |
| `agents/trigger.md` | `agents/` | 内置 `trigger` 子代理定义（system prompt） |
| `agents/executor.md` | `agents/` | 内置 `executor` 子代理定义（声明唯一的 `tools: executor` 增量） |
| `references/connector.md` | `references/` | 跨子代理共享的连接器/source 使用契约（非 skill，见下节） |

### 仅嵌入、不落盘

`identity.md` 与 `dream.md` 只作为嵌入模板使用，**不再写入 workspace**：

- `identity.md` 由 `ContextBuilder` 直接读取嵌入内容作为 system prompt 的固定开头
- `dream.md` 由 `DreamService` 读取并注入对话历史后作为 Dream 提示词

### 技能文件

创建到 `workspace/skills/` 目录：

| 文件 | 说明 |
|------|------|
| `<name>/SKILL.md` | 可复用工作流（当前内置 `memory` 技能） |

### Reference 文档不进 `Available Skills`

`references/` 是与 `skills/`、`agents/` 并列的工作区目录，但两者落盘路径完全不同：

- `resources/templates/references/connector.md` → workspace `references/connector.md`，经 `bootstrap_files()` 落盘（`build.rs` 递归扫 `resources/templates/**`，子目录名直接拼 dest 前缀，所以新目录零构建改动）。
- `skill_files()` 只捞 `dest.starts_with("skills/")`，而 `# Available Skills` 只由 workspace `skills/` 目录渲染 ⇒ `references/` 下的文档天然不会出现在任何 system prompt 的 skill 列表里。

这是这个目录存在的理由：skill 的 available 列表注入**每轮** system prompt（含主循环），一份只该被子代理读的连接器契约放进去就是主 loop 污染。reference 走「`agents/*.md` 正文里一行硬指针 + 按需 `file_reader`」：agent.md 正文原样作子代理 system prompt，指针因此只在子代理上下文生效、从不进主循环（见 [agent.md](agent.md#提示词注入)）。

### reference 也是受保护文档

`protected_documents()` 从 `bootstrap_files()` 派生 ⇒ `references/connector.md` 与 `agents/*.md` 同类：它是宿主 authored 的知识，`file_writer` / `file_editor` 拿到 `REFUSED:`（`src/tools/protected.rs`，ADR 0012）。**这是有意的**——子代理不能自己改写连接器契约（否则一次运行就能改掉下一次运行的规则）；要改 reference 就走 owner 的模板与重新 `setup`。

## 模板枚举

```rust
pub fn bootstrap_files() -> impl Iterator<Item = (&'static str, &'static str)>
pub fn skill_files()     -> impl Iterator<Item = (&'static str, &'static str, &'static str)>
```

- `bootstrap_files()` 返回 8 个文件，元素为 `(workspace 相对路径, 内容)` —— `MEMORY.md` 的路径是 `memory/MEMORY.md`，两个内置 agent 定义在 `agents/`，共享 reference 在 `references/`
- `protected_documents()` 从 `bootstrap_files()` 派生（减去 `MEMORY.md`）：新加一个模板文件就自动成为受保护文档，不需要第二份名单
- `skill_files()` 返回嵌入技能文件，元素为 `(filename, content, dest_path)`，`dest_path` 以 `skills/` 开头

## 未修改模板不注入

「文件内容与模板一致则跳过」的语义不再由 bootstrap 层承担，而是由 `MemoryService::get_effective()` 承接（见 [memory.md](memory.md)）。`ContextBuilder` 按 category 读取，`get_effective` 对缺失 / 空 / 等于模板的文档返回 `None`。

## 新增模板不会自动出现在已有 workspace

`setup` 的语义是「已存在就跳过」，gateway 启动也不补文件。所以往 `bootstrap_files()` 里加一个模板（如 `references/connector.md`）只对**新建 workspace** 生效；已有部署要拿到它必须重跑一次 `cargo run -- setup`。这条坑在收口文档时要一起提。

## 嵌入资源

```rust
pub fn get_content_by_dest(dest_path: &str) -> Option<&'static str>
```

按模板的相对路径（如 `AGENTS.md`、`dream.md`、`skills/memory/SKILL.md`）查询嵌入内容。

## 嵌入机制

`embed.rs` 使用 `include!(concat!(env!("OUT_DIR"), "/embed_gen.rs"))` 引入构建时生成的代码。`build.rs` 扫描 `resources/templates/`（前缀 `""`）、`resources/skills/`（前缀 `"skills/"`）与 `resources/webui/`（前缀 `"webui/"`）并生成 Rust 代码。
