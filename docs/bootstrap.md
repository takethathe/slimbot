# Bootstrap & Embedded Files

对应模块：`src/bootstrap.rs` + `src/embed.rs`

## 概述

`bootstrap.rs` 提供嵌入的模板文件访问，`embed.rs` 通过 `include!` 引入构建时生成的嵌入文件代码。

## Bootstrap 模板

### 核心文件

启动时自动创建到 workspace 目录的文件（若不存在）：

| 文件 | 说明 |
|------|------|
| `AGENTS.md` | Agent 行为定义 |
| `USER.md` | 用户画像 |
| `SOUL.md` | Agent 人格 |
| `TOOLS.md` | 工具使用指南 |

### 技能文件

创建到 `workspace/skills/` 目录：

| 文件 | 说明 |
|------|------|
| `SKILL.md` | 记忆系统使用指南 |

## 嵌入资源

```rust
pub fn embedded_files() -> &'static [(&'static str, &'static str, &'static str)]
```

返回所有嵌入资源：`(filename, content, dest_path)`。

## `read_if_modified`

```rust
pub fn read_if_modified(path: &Path, template: &str) -> Option<String>
```

智能读取文件：
- 文件不存在 → `None`
- 文件内容与模板相同 → `None`（跳过，节省 token）
- 文件内容不同 → `Some(content)`

`ContextBuilder` 使用此函数加载工作区文件，避免加载未修改的模板文件。

## Skill 文件分离

```rust
pub fn bootstrap_files()   // 核心模板（workspace 根目录）
pub fn skill_files()       // 技能文件（workspace/skills/ 目录）
```

两种文件通过 `dest_path` 是否以 `skills/` 开头来区分。

## 嵌入机制

`embed.rs` 使用 `include!(concat!(env!("OUT_DIR"), "/embed_gen.rs"))` 引入构建时生成的代码。嵌入文件由 `build.rs` 从 `bootstrap/` 目录读取并生成 Rust 代码。
