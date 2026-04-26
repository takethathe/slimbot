# 配置指南

本文档描述如何配置 slimbot。

## 快速开始

运行 setup 命令生成默认配置文件：

```bash
cargo run -- setup          # 创建 ~/.slimbot/config.json
cargo run -- setup /path/to/custom.json   # 自定义路径
```

如果配置文件已存在，`setup` 会加载它，填补缺失的默认值，规范化无效条目，然后写回。

## 配置文件位置

| 方式 | 路径 |
|------|------|
| 默认（无参数） | `~/.slimbot/config.json` |
| CLI 参数 | `cargo run -- <config.json路径>` |
| Setup 命令 | `cargo run -- setup [config.json路径]` |

## 数据目录

会话数据和工作区文件存储在不同的目录中。目录结构如下：

```
~/.slimbot/                         # data_dir（运行时/会话数据）
└── workspace/                      # workspace_dir（Agent 文件、技能、会话）
    ├── agent.md                    # Agent 行为定义
    ├── user.md                     # 用户画像
    ├── soul.md                     # Agent 人格
    ├── tools.md                    # 工具使用指南
    ├── skills/                     # 可选技能文件 (*.md)
    └── sessions/
        └── {session_id}.jsonl      # 会话消息持久化
```

`data_dir` 和 `workspace_dir` 是两个独立的配置项。默认情况下，`workspace_dir` 为 `{data_dir}/workspace`。

## 配置结构

```json
{
  "data_dir": "~/.slimbot",
  "workspace_dir": "~/.slimbot/workspace",
  "agent": { ... },
  "providers": { ... },
  "tools": [ ... ],
  "channels": [ ... ]
}
```

### 顶层字段

| 字段 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `data_dir` | string | 否 | `~/.slimbot` | 运行时会话数据的基目录 |
| `workspace_dir` | string | 否 | `{data_dir}/workspace` | Agent 文件、技能和会话的目录 |
| `agent` | object | **是** | — | 单个 Agent 配置 |
| `providers` | object | **是** | — | 命名 Provider 定义（键值映射） |
| `tools` | array | 否 | `[]` | 注册的工具定义列表 |
| `channels` | array | 否 | `[]` | 通信通道定义列表 |

### `agent` — Agent 配置

SlimBot 只有一个 Agent。Agent 通过名称从 `providers` 映射中引用一个 Provider。

| 字段 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `provider` | string | **是** | — | 要使用的 Provider 名称（必须匹配 `providers` 中的一个键） |
| `max_iterations` | uint | 否 | `40` | 每轮最大工具调用迭代次数 |
| `timeout_seconds` | uint | 否 | `120` | 每轮 Agent 超时时间（秒） |

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

Provider 级别字段：

| 字段 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `type` | string | 否 | `"openai"` | Provider 类型：`"openai"` 或 `"custom"` |
| `api_url` | string | 否 | 见下文 | 完整 API 端点 URL |
| `base_url` | string | 否 | — | Provider 基础 URL（例如 `"https://api.openai.com"`） |
| `api_key` | string | **是** | — | API 认证密钥（加载时验证） |
| `model` | string | **是** | — | 模型名称（加载时验证） |
| `temperature` | float | 否 | `0.7` | 采样温度（0.0–2.0） |
| `max_tokens` | uint | 否 | `4096` | 单次最大响应 token 数 |

**URL 解析逻辑：** `api_url` 优先级更高。如果为空，则从 `base_url + "/v1/chat/completions"` 推导。如果两者都未设置，默认为 `https://api.openai.com/v1/chat/completions`。

**Provider 类型：**

- `"openai"` — 默认。使用 OpenAI API 端点。
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

列出 Agent 可用的工具。如果数组为空，默认启用全部 6 个内置工具。

可用工具：`shell`、`file_reader`、`file_writer`、`file_editor`、`list_dir`、`make_dir`。

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

### `channels` — 通信通道

定义 Agent 的输入/输出通道。每个条目指定一个通道类型和可选配置。

| 字段 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `type` | string | **是** | — | 通道类型标识符（例如 `"cli"`） |
| `enabled` | bool | 否 | `true` | 是否启用该通道 |
| `config` | object | 否 | `{}` | 通道特定配置（任意 JSON） |

```json
{
  "channels": [
    { "type": "cli", "enabled": true, "config": {} }
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

## 规范化

对已有配置运行 `cargo run -- setup` 时，应用以下规范化规则：

- 空 `data_dir` → `~/.slimbot`
- 空 `workspace_dir` → `{data_dir}/workspace`
- 空 `agent.provider` → `"default"`
- 空 `agent.max_iterations` → `40`
- 空 `agent.timeout_seconds` → `120`
- 对每个 Provider：
  - 空 `type` → `"openai"`
  - 空 `api_url` 且设置了 `base_url` → `base_url + "/v1/chat/completions"`
  - `api_url` 和 `base_url` 都为空 → `https://api.openai.com/v1/chat/completions`
  - 空 `model` → `gpt-4o`
  - `temperature` ≤ 0.0 或 > 2.0 → `0.7`
  - `max_tokens` = 0 → `4096`
  - `api_key` **永不**被规范化或覆盖
- `name`/`type` 为空的 tools/channels 条目会被移除

## 完整示例

```json
{
  "data_dir": "/Users/takethat/.slimbot",
  "workspace_dir": "/Users/takethat/.slimbot/workspace",
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
  "channels": [
    { "type": "cli", "enabled": true, "config": {} }
  ]
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
