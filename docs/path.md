# Path Manager

对应模块：`src/path.rs`

## 概述

`PathManager` 管理 SlimBot 的所有文件路径，提供路径解析、默认值填充和安全验证。

## 路径解析优先级

### data_dir

1. CLI 参数 `--data-dir` 指定
2. 默认值 `~/.slimbot`

### workspace_dir

1. CLI 参数 `--workspace-dir` 指定
2. 默认值 `{data_dir}/workspace`

### config_path

1. CLI 参数 `--config` 或位置参数指定（必须存在）
2. 默认值 `{data_dir}/config.json`

## 结构

```rust
pub struct PathManager {
    config_path: PathBuf,
    data_dir: PathBuf,
    workspace_dir: PathBuf,
}
```

## 公共方法

### `resolve`

```rust
pub fn resolve(
    config: Option<&str>,
    data_dir: Option<&str>,
    workspace_dir: Option<&str>,
) -> Result<Self>
```

解析并验证所有路径：
- 创建缺失的目录
- 规范化路径（`canonicalize`）
- 验证 workspace_dir 必须在 data_dir 之下（当显式指定时）

### 访问器

| 方法 | 返回 | 说明 |
|------|------|------|
| `config_path()` | `&Path` | 配置文件路径 |
| `data_dir()` | `&Path` | 数据目录 |
| `workspace_dir()` | `&Path` | 工作区目录 |
| `session_dir()` | `PathBuf` | `{workspace}/sessions` |
| `skills_dir()` | `PathBuf` | `{workspace}/skills` |
| `memory_dir()` | `PathBuf` | `{workspace}/memory` |
| `tool_results_dir()` | `PathBuf` | `{workspace}/.tool_results` |
| `bootstrap_file(name)` | `PathBuf` | `{workspace}/{name}` |

### `validate_path_sandbox`

```rust
pub fn validate_path_sandbox(&self, user_path: &str) -> Result<PathBuf>
```

验证用户提供的路径不超出 workspace 目录。防止路径遍历攻击（如 `../escape.txt`）：
- 解析为绝对路径
- 检查是否在 workspace 之下
- 对不存在的路径，回溯到最近的已存在祖先目录进行验证

### `expand_home`

```rust
pub fn expand_home(path: &str) -> PathBuf
```

将 `~` 或 `~/...` 展开为用户主目录：
- `~` → `/home/user`
- `~/.slimbot` → `/home/user/.slimbot`
- 非 `~` 开头的路径原样返回
