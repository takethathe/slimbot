# 配置指南

本文档描述如何配置 slimbot。

## 快速开始

运行 setup 命令生成默认配置文件：

```bash
cargo run -- setup                     # 创建 ~/.slimbot/config.json
cargo run -- -d /custom/path setup     # 自定义数据目录
cargo run -- -c /path/to/custom.json setup  # 自定义配置文件路径
```

如果配置文件已存在，`setup` 会加载它，填补缺失的默认值，规范化无效条目，然后写回。

## 命令行参数

| 参数 | 简写 | 默认值 | 说明 |
|------|------|--------|------|
| `--config PATH` | `-c` | `{data-dir}/config.json` | 配置文件路径（必须存在） |
| `--data-dir PATH` | `-d` | `~/.slimbot` | 应用数据目录 |
| `--workspace-dir PATH` | `-w` | `{data-dir}/workspace` | 工作区目录 |

**向后兼容：** 第一个位置参数仍可作为配置文件路径（`cargo run -- config.json`）。

### 路径推断逻辑

- 未指定 `--data-dir` 时，默认为 `~/.slimbot`
- 未指定 `--workspace-dir` 时，从 `--data-dir` 推断为 `{data-dir}/workspace`
- 未指定 `--config` 时，默认为 `{data-dir}/config.json`
- 如果显式指定了 `--config` 但文件不存在，程序会退出并报错

### 使用示例

```bash
# 使用默认路径
cargo run --

# 指定配置文件（向后兼容的位置参数）
cargo run -- /path/to/config.json

# 指定数据目录，工作区自动推断为 /data/myapp/workspace
cargo run -- -d /data/myapp

# 完全自定义所有路径
cargo run -- -c /etc/slimbot/config.json -d /opt/slimbot -w /opt/slimbot/ws

# Setup 命令使用自定义路径
cargo run -- -d /data/myapp setup
```

## 数据目录

会话数据和工作区文件存储在不同的目录中。目录结构如下：

```
{data-dir}/                           # 应用数据目录（--data-dir）
└── workspace/                        # 工作区目录（--workspace-dir 或默认推断）
    ├── AGENTS.md                     # Agent 行为定义
    ├── USER.md                       # 用户画像
    ├── SOUL.md                       # Agent 人格
    ├── TOOLS.md                      # 工具使用指南
    ├── skills/                       # 可选技能文件 (*.md)
    ├── memory/                       # MEMORY.md, history.jsonl
    └── sessions/
        └── {session_id}.jsonl        # 会话消息持久化
```

`data-dir` 和 `workspace-dir` 通过命令行参数配置，不在 config.json 中存储。

## 配置结构

```json
{
  "agent": { ... },
  "providers": { ... },
  "tools": [ ... ],
  "channels": { ... },
  "gateway": { ... }
}
```

### 顶层字段

| 字段 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `agent` | object | **是** | — | 单个 Agent 配置 |
| `providers` | object | **是** | — | 命名 Provider 定义（键值映射） |
| `tools` | array | 否 | `[]` | 注册的工具定义列表 |
| `channels` | object | 否 | `{}` | 通信通道定义（键值映射） |
| `gateway` | object | 否 | 见下文 | Gateway 模式配置 |

### `channels` — 通信通道

定义 Agent 的输入/输出通道。每个条目是一个键值对，**键为通道类型标识符**（如 `"webui"`），值为通道配置。

**注意：** CLI 通道不需要在 `channels` 中配置。CLI 模式始终隐式启用 CLI 通道，仅用于非 Gateway 模式的交互式终端会话。Gateway 模式不启动 CLI 通道。

| 字段 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `enabled` | bool | 否 | `true` | 是否启用该通道 |
| 其他 | any | 否 | — | 通道特定的任意配置字段（通过 `#[serde(flatten)]` 捕获） |

可用通道类型：

- **`webui`** — Web 界面通道，内置 HTTP 服务器（基于 axum），支持 SSE 流式输出。配置字段见下表。

#### WebUI 通道配置

| 字段 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `host` | string | 否 | `"0.0.0.0"` | 监听地址 |
| `port` | integer | 否 | `8080` | 监听端口 |

```json
{
  "channels": {
    "webui": {
      "enabled": true,
      "host": "127.0.0.1",
      "port": 8080
    }
  }
}
```

**向后兼容：** 仍支持旧的数组格式（`[{ "type": "cli", ... }]`），加载时会自动转换为 HashMap 格式。旧格式中的 `type` 字段会被忽略，配置键即为通道类型。

### `gateway` — Gateway 模式配置

控制 `slimbot gateway` 模式下启动的后台服务。

```json
{
  "gateway": {
    "cron": {
      "enabled": true
    },
    "heartbeat": {
      "enabled": true,
      "interval_s": 1800
    }
  }
}
```

| 子字段 | 类型 | 默认值 | 说明 |
|--------|------|--------|------|
| `cron.enabled` | bool | `true` | 是否启用 cron 定时调度 |
| `heartbeat.enabled` | bool | `true` | 是否启用 heartbeat 定期检查 |
| `heartbeat.interval_s` | uint | `1800` | heartbeat 检查间隔（秒），默认 30 分钟 |

### `agent` — Agent 配置

SlimBot 只有一个 Agent。Agent 通过名称从 `providers` 映射中引用一个 Provider。

| 字段 | 类型 | 约束 | 默认值 | 说明 |
|------|------|------|--------|------|
| `provider` | string | 归一化 | `""` → `"default"` | 要使用的 Provider 名称（引用 `providers` 中的一个键） |
| `max_iterations` | uint | range(1, 200) | `40` | 每轮最大工具调用迭代次数 |
| `timeout_seconds` | uint | range(1, 600) | `120` | 每轮 Agent 超时时间（秒） |
| `max_tool_result_chars` | uint | range(100, 100000) | `8000` | 工具返回结果的最大字符数，超过此值会被截断 |
| `persist_tool_results` | bool | — | `true` | 超大工具结果是否持久化到磁盘 |
| `context_window_tokens` | uint | range(1024, 200000) | `8192` | LLM 上下文窗口大小（token），用于 Consolidator 预算检查 |

```json
{
  "agent": {
    "provider": "my_provider_a",
    "max_iterations": 20,
    "timeout_seconds": 60
  }
}
```

### `providers` — LLM Provider 定义

一个命名 Provider 的键值映射。每个 Provider 可以通过其键名被 Agent 引用。

| 字段 | 类型 | 约束 | 默认值 | 说明 |
|------|------|------|--------|------|
| `type` | string | allowed(["openai", "anthropic", "custom"]) | `"openai"` | Provider 类型标识符 |
| `api_url` | string | str_max(512), 归一化 | 见下文 | 完整 API 端点 URL |
| `base_url` | string | str_max(512) | `"https://api.openai.com"` | Provider 基础 URL |
| `api_key` | string | str_max(512) | `""` | API 认证密钥（加载时验证） |
| `model` | string | str_max(128) | `"gpt-4o"` | 模型标识符（加载时验证） |
| `temperature` | float | range(0.0, 2.0) | `0.7` | 采样温度 |
| `max_tokens` | uint | range(1, 100000) | `4096` | 单次最大响应 token 数 |
| `prompt_cache_enabled` | bool | — | `true` | 是否启用 prompt 缓存 |

**URL 解析逻辑（归一化）：** `api_url` 优先级更高。如果为空，则从 `base_url` 推导：
- `base_url` 以 `/chat/completions` 结尾 → 直接使用
- `base_url` 以 `/v1` 结尾 → 追加 `/chat/completions`
- 其他情况 → 追加 `/v1/chat/completions`

如果两者都为空，默认为 `https://api.openai.com/v1/chat/completions`。

**Provider 类型：**

- `"openai"` — 默认。使用 OpenAI API 端点。
- `"anthropic"` — Anthropic Claude API。
- `"custom"` — 任意 OpenAI 兼容 API。将 `base_url` 设置为你自己的 Provider 根 URL。

多 Provider 示例：

```json
{
  "providers": {
    "my_provider_a": {
      "type": "custom",
      "base_url": "https://api.siliconflow.cn",
      "api_key": "sk-your-api-key",
      "model": "Qwen/Qwen2.5-72B-Instruct"
    },
    "my_provider_b": {
      "type": "openai",
      "api_key": "sk-openai-key",
      "model": "gpt-4o"
    }
  }
}
```

### `tools` — 工具定义

列出 Agent 可用的工具。如果数组为空，默认启用全部内置工具。

| 字段 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `name` | string | **是** | — | 工具标识符（完整描述参见 [docs/tools.md](tools.md)） |
| `enabled` | bool | 否 | `true` | 是否启用该工具 |

```json
{
  "tools": [
    { "name": "shell", "enabled": true },
    { "name": "file_reader", "enabled": true },
    { "name": "file_editor", "enabled": false }
  ]
}
```

## 最小配置

仅需配置 `agent.provider` 及其引用的 Provider 的 `api_key` 和 `model`：

```json
{
  "agent": {
    "provider": "default"
  },
  "providers": {
    "default": {
      "api_key": "sk-your-api-key",
      "model": "gpt-4o"
    }
  }
}
```

## 验证规则

加载配置时强制执行以下验证：

- `agent.provider` 不能为空
- 引用的 Provider 必须存在于 `providers` 中
- 引用的 Provider 的 `api_key` 不能为空
- 引用的 Provider 的 `model` 不能为空

如果验证失败，应用程序将打印错误消息并退出。

## 值约束（Clamp）

配置项通过 `define_config!` 宏定义，内嵌取值范围元数据。加载时自动执行：

- **数值范围** — 超出范围时 clamp 到边界值（如 `max_iterations: 9999` → `200`）
- **字符串最大长度** — 超过时截断（如 `api_url` 最大 512 字符）
- **白名单值** — 不在列表中时回退到默认值（如 `type: "invalid"` → `"openai"`）

具体约束见上方各字段的 "约束" 列。

## 规范化

加载配置后，对每个模块执行 `normalize()` 方法（通过 `Normalizable` trait 实现）：

- 空 `agent.provider` → `"default"`
- 对每个 Provider：
  - 空 `api_url` 且设置了 `base_url` → 根据 `base_url` 结尾推导（见上方 URL 解析逻辑）
  - `api_url` 和 `base_url` 都为空 → `https://api.openai.com/v1/chat/completions`
  - `api_key` **永不**被规范化或覆盖
- `name` 为空的 tools 条目会被移除；channels 按空键名过滤

规范化在 clamp 之后执行，确保归一化过程产生的值也经过约束检查。

## 全局单例与热重载

配置通过全局单例访问：`Config::init(path)` 初始化，`Config::get()` 返回当前配置快照。

### 文件监听

初始化后自动启动文件系统监听（基于 `notify` crate，macOS 使用 FSEvents）。当 `config.json` 被修改时：

1. 重新读取文件，执行 clamp + normalize + validate
2. 计算新旧配置差异（diff），生成 `ConfigChange { paths, old_values, new_values }`
3. 通知匹配的订阅者（基于路径前缀匹配）

### 订阅机制

模块可注册回调监听特定配置路径的变化：

```rust
Config::subscribe("agent", |change| {
    println!("Agent config changed: {:?}", change.paths);
});
```

订阅的路径前缀匹配规则：`"agent"` 会匹配所有以 `agent.` 开头的变更路径（如 `agent.max_iterations`）。

### 向前兼容

未知 JSON 字段通过 `#[serde(flatten)]` 保留在配置的 `unknown` 字段中。这意味着：
- 未来版本添加的新字段在旧版本加载时不会报错
- 保存时未知字段会原样写回 JSON 文件

## 完整示例

```json
{
  "agent": {
    "provider": "siliconflow",
    "max_iterations": 40,
    "timeout_seconds": 120
  },
  "providers": {
    "siliconflow": {
      "type": "custom",
      "base_url": "https://api.siliconflow.cn",
      "api_key": "sk-your-api-key-here",
      "model": "Qwen/Qwen2.5-72B-Instruct",
      "temperature": 0.7,
      "max_tokens": 4096
    }
  },
  "tools": [
    { "name": "shell", "enabled": true }
  ],
  "channels": {
    "webui": { "enabled": true, "host": "127.0.0.1", "port": 8080 }
  },
  "gateway": {
    "cron": { "enabled": true },
    "heartbeat": { "enabled": true, "interval_s": 1800 }
  }
}
```

## 多 Provider 示例

定义多个 Provider，通过修改 `agent.provider` 来切换：

```json
{
  "agent": {
    "provider": "my_provider_a"
  },
  "providers": {
    "my_provider_a": {
      "type": "custom",
      "api_key": "xxxx",
      "base_url": "xxxx"
    },
    "my_provider_b": {
      "type": "openai",
      "api_key": "sk-openai-key",
      "model": "gpt-4o"
    }
  }
}
```
