# Bootstrap & Embedded Files

对应模块：`src/bootstrap.rs` + `src/embed.rs`

## 概述

`bootstrap.rs` 提供嵌入模板文件的访问，`embed.rs` 通过 `include!` 引入构建时生成的嵌入文件代码。

## Bootstrap 模板

### 记忆文件

`setup` 时自动创建到 workspace 的 5 个记忆文件（若不存在）：

| 文件 | 写入位置 | 说明 |
|------|----------|------|
| `AGENTS.md` | workspace 根 | Agent 工作指令 |
| `USER.md` | workspace 根 | 用户画像 |
| `SOUL.md` | workspace 根 | Agent 人格 |
| `TOOLS.md` | workspace 根 | 工具使用指南 |
| `MEMORY.md` | `memory/` | 长期记忆（初始为模板，未修改时不注入） |

### 仅嵌入、不落盘

`identity.md` 与 `dream.md` 只作为嵌入模板使用，**不再写入 workspace**：

- `identity.md` 由 `ContextBuilder` 直接读取嵌入内容作为 system prompt 的固定开头
- `dream.md` 由 `DreamService` 读取并注入对话历史后作为 Dream 提示词

### 技能文件

创建到 `workspace/skills/` 目录：

| 文件 | 说明 |
|------|------|
| `<name>/SKILL.md` | 可复用工作流（当前内置 `memory` 技能） |

## 模板枚举

```rust
pub fn bootstrap_files() -> impl Iterator<Item = (&'static str, &'static str)>
pub fn skill_files()     -> impl Iterator<Item = (&'static str, &'static str, &'static str)>
```

- `bootstrap_files()` 返回 5 个记忆文件，元素为 `(workspace 相对路径, 内容)` —— `MEMORY.md` 的路径是 `memory/MEMORY.md`
- `skill_files()` 返回嵌入技能文件，元素为 `(filename, content, dest_path)`，`dest_path` 以 `skills/` 开头

## 未修改模板不注入

「文件内容与模板一致则跳过」的语义不再由 bootstrap 层承担，而是由 `MemoryService::get_effective()` 承接（见 [memory.md](memory.md)）。`ContextBuilder` 按 category 读取，`get_effective` 对缺失 / 空 / 等于模板的文档返回 `None`。

## 嵌入资源

```rust
pub fn get_content_by_dest(dest_path: &str) -> Option<&'static str>
```

按模板的相对路径（如 `AGENTS.md`、`dream.md`、`skills/memory/SKILL.md`）查询嵌入内容。

## 嵌入机制

`embed.rs` 使用 `include!(concat!(env!("OUT_DIR"), "/embed_gen.rs"))` 引入构建时生成的代码。`build.rs` 扫描 `resources/templates/`（前缀 `""`）、`resources/skills/`（前缀 `"skills/"`）与 `resources/webui/`（前缀 `"webui/"`）并生成 Rust 代码。
