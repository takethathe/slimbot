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
  "tools": { "entries": [ ... ], "approval": { ... } },
  "channels": { ... },
  "gateway": { ... },
  "json_render": { ... },
  "pressure": { ... }
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
| `json_render` | object | 否 | 见下文 | WebUI 卡片生成配置 |
| `pressure` | object | 否 | 见下文 | 上下文压力占比（长度限制全部由 provider 窗口派生） |

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
| `host` | string | 否 | `"127.0.0.1"` | 监听地址，**只允许 `127.0.0.1` / `localhost` / `::1`**；其他值直接启动失败（见 ADR 0010） |
| `port` | integer | 否 | `8080` | 监听端口 |

> **Breaking change（ADR 0010）**：默认 `host` 从 `0.0.0.0` 改为 `127.0.0.1`，且不再接受任何非 loopback 值。WebUI 是纯本机入口、无认证；远程访问请用 IM 通道。以 `0.0.0.0` 配置的实例升级后**启动会失败**并给出修复提示。

WebUI 为补偿「无认证」在 HTTP 层强制四条护栏（全局中间件，不逐 handler）：`Host` 必须是 loopback（否则 `421`，封死 DNS rebinding）；变更请求必须带同源 `Origin`（缺失即 `403`）、必须是 `application/json`（否则 `415`）、必须带 `Sec-Fetch-Site: same-origin`（否则 `403`）；且不返回任何 CORS 头。脚本/curl 调变更端点时必须显式带上这四个头。

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
    "owner": { "id": "owner", "display_name": null },
    "cron": {
      "enabled": true
    },
    "heartbeat": {
      "enabled": true,
      "interval_s": 1800
    },
    "dream": {
      "enabled": true,
      "interval_ms": 7200000,
      "model_override": null,
      "max_entries": 20
    }
  }
}
```

| 子字段 | 类型 | 默认值 | 说明 |
|--------|------|--------|------|
| `owner.id` | string | `"owner"` | 单用户的稳定标识：记忆、审批、投递都以它为归属（v1 永久单用户） |
| `owner.display_name` | string \| null | `null` | owner 的展示名（可选，仅用于显示） |
| `cron.enabled` | bool | `true` | 是否启用 cron 定时调度 |
| `heartbeat.enabled` | bool | `true` | 是否启用 heartbeat 定期检查 |
| `heartbeat.interval_s` | uint | `1800` | heartbeat 检查间隔（秒），默认 30 分钟 |
| `dream.enabled` | bool | `true` | 是否启用 Dream 定时记忆整理 |
| `dream.interval_ms` | int | `7200000` | Dream 触发间隔（毫秒），默认 2 小时 |
| `dream.model_override` | string \| null | `null` | 可选：指定用于 Dream 的 provider 名称（覆盖主 agent 的 provider） |
| `dream.max_entries` | uint | `20` | 每次 Dream 最多处理的 history 条目数 |

**Dream 说明：** Dream 服务定期分析 `memory/history.jsonl` 中未处理的对话历史，将关键事实整理到长期记忆文件（SOUL.md, USER.md, MEMORY.md, skills/）。可通过 `/dream` 命令手动触发。Dream 完成后会清理已处理的 history 条目（保留最近 50 条）。

### `json_render` — WebUI 卡片生成

控制一次 Run 结束后独立的卡片生成调用（机制见 `docs/channels.md` 「卡片下发」，决策见 `docs/adr/0005`）。缺段时用默认值。

| 字段 | 类型 | 默认值 | 范围 | 说明 |
|------|------|--------|------|------|
| `enabled` | bool | `true` | — | 卡片生成总开关（真正的门依旧是 Catalog 注册：未注册的会话从不产生卡片） |
| `model_override` | string \| null | `null` | — | 可选：指定卡片生成用的 provider 名称（默认用主 agent 的 provider，未命中时回退） |
| `max_tokens` | uint | `2048` | 64–32768 | 单次卡片生成调用的输出 token 预算（覆盖所用 provider 的 `max_tokens`） |
| `timeout_seconds` | uint | `30` | 1–300 | 单次卡片生成调用的**无进展窗口**（秒）：流式信号（内容增量或存活心跳）到达即续命，窗口内毫无信号才判停滞，静默降级为 `failed` 事件 |

**说明：** 卡片 provider 的身份（base_url/api_key/model）来自 `model_override` 命中的 provider 或主 agent provider，`max_tokens` 恒为 `json_render.max_tokens` ，且 `disable_thinking` 恒为 true（卡片是机械的转录→JSONL 转换，思考只会拖慢速度并拉长首信号等待，主 agent 对话不受此影响）。启动该调用的 `prompt_cache_enabled` 同样恒为 true（即使 provider 上显式写了 `false` 也覆盖；Catalog prompt 是卡片调用唯一稳定的前缀），请求一律以 `stream: true` 发起；两者都不回写被继承的 provider 配置，也不影响主 agent 对话。

### `agent` — Agent 配置

SlimBot 只有一个 Agent。Agent 通过名称从 `providers` 映射中引用一个 Provider。

| 字段 | 类型 | 约束 | 默认值 | 说明 |
|------|------|------|--------|------|
| `provider` | string | 归一化 | `""` → `"default"` | 要使用的 Provider 名称（引用 `providers` 中的一个键） |
| `max_iterations` | uint | range(1, 200) | `40` | 每轮最大工具调用迭代次数 |
| `timeout_seconds` | uint | range(1, 600) | `120` | 每轮 Agent 超时时间（秒） |
| `persist_tool_results` | bool | — | `true` | 超大工具结果是否持久化到磁盘（阈值由 `pressure` 派生） |

`agent` 段**不再**包含任何长度/窗口字段：窗口是模型属性（见 `providers.context_window_tokens`），长度是窗口的百分比（见 `pressure`）。

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
| `api_key` | string | secret（掩码，不截断） | `""` | API 认证密钥（加载时验证；WebUI 中掩码显示，留空 = 不变。留空时会去 keychain / 环境变量查找，见「密钥与凭证」） |
| `model` | string | str_max(128) | `"gpt-4o"` | 模型标识符（加载时验证） |
| `temperature` | float | range(0.0, 2.0) | `0.7` | 采样温度 |
| `max_tokens` | uint | range(1, 100000) | `4096` | 单次最大响应 token 数 |
| `context_window_tokens` | uint | range(1024, 200000) | `32768` | 该模型的上下文窗口（token）。上下文压力唯一的绝对锚，其余长度限制都是它的百分比 |
| `prompt_cache_enabled` | bool | — | `true` | 是否启用 prompt 缓存 |
| `disable_thinking` | bool | — | `false` | 强制禁用该 provider 调用的思考/推理模式（请求体注入 `enable_thinking: false`，DashScope 兼容接口字段；卡片生成路径恒为 true） |

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

### `pressure` — 上下文压力占比

上下文压力只有一个绝对锚：provider 的 `context_window_tokens`。其余每个长度限制都是该窗口的百分比（决策见 `docs/adr/0013-context-pressure-single-anchor.md`）。换更大的模型只需改 provider 的窗口，所有限制随之等比放大。

| 字段 | 类型 | 约束 | 默认值 | 说明 |
|------|------|------|--------|------|
| `single_tool_result_ratio` | float | range(0.01, 0.90) | `0.25` | 单条工具结果最多占窗口的比例（换算为字符后用于截断） |
| `persisted_preview_ratio` | float | range(0.001, 0.50) | `0.04` | 落盘结果在上下文中保留的预览长度（占窗口比例） |
| `snip_safety_ratio` | float | range(0.001, 0.50) | `0.03` | 预留给 tokenizer 估算误差的比例 |
| `consolidate_trigger_ratio` | float | range(0.05, 0.95) | `0.50` | prompt token 超过窗口的该比例时触发会话摘要 |
| `consolidate_target_ratio` | float | range(0.05, 1.0) | `0.60` | 摘要把 prompt 压到触发点的该比例 |
| `summary_ratio` | float | range(0.001, 0.50) | `0.02` | 单条摘要允许占窗口的比例（换算为字符） |
| `min_keep_messages` | uint | range(1, 500) | `4` | snip 至少保留的非 system 消息条数（找不到 User 时的兜底） |
| `keep_recent_tool_results` | uint | range(1, 500) | `10` | 最近多少条工具结果不参与残留清理 |
| `tool_results_ttl_days` | uint | range(1, 365) | `7` | `tool-results/` 落盘文件的保留天数，由 Dream 成功收尾时清扫（无独立定时器） |

```json
{
  "providers": {
    "default": { "type": "openai", "api_key": "sk-...", "model": "gpt-4o", "context_window_tokens": 128000 }
  },
  "pressure": {
    "single_tool_result_ratio": 0.25,
    "consolidate_trigger_ratio": 0.5
  }
}
```

**冷启动比例：** 会话还没有真实用量时，字符/ token 换算比例默认取 `3.0`（中英混排的保守值）；首次收到 provider 返回的 `prompt_tokens` 后按会话实测值替换，并随会话元数据持久化。窗口为 `0` 时所有派生长度限制都退化为 `0`，各阶段（截断、清理、摘要）都不动手，而不是瞎猜。

**绝对下限/上限：** 派生值仍会被夹住，避免极小窗口把限制压成不可用的数字——单条工具结果 1,000–64,000 字符、落盘预览 256–8,000 字符、snip 安全余量 128–8,192 token、摘要 256–8,192 字符。

#### 迁移说明（两条 breaking change）

1. `context_window_tokens` 从 `agent` 段移到 **provider** 段。启动时若发现旧的 `agent.context_window_tokens`，会以一行 `[config migration]` 日志说明迁移动作，把它搬到 `providers.<agent.provider>`（该 provider 已有自己的窗口值时保留 provider 的值），并在加载后从配置里删除旧键；`setup` 会把迁移结果写回文件。
2. `agent.max_tool_result_chars` 已废弃。旧键会被忽略并给出同样的迁移日志；原来的一个字段现在拆成两个比例——"单条结果上限"（`pressure.single_tool_result_ratio`，也是落盘阈值）与"落盘预览长度"（`pressure.persisted_preview_ratio`）。

```json
// 旧配置（仍可启动，但会打印迁移日志）
{ "agent": { "context_window_tokens": 65536, "max_tool_result_chars": 16000 }, "providers": { "default": { ... } } }

// 迁移后
{ "providers": { "default": { "context_window_tokens": 65536, ... } }, "pressure": { ... } }
```

### `tools` — 工具定义与审批策略

`tools` 有两个子段：`entries`（启用哪些工具）与 `approval`（风险级的管控档）。历史形态（直接把工具数组放在 `tools` 下）仍然可以加载，等价于 `{"entries": [...]}`。

#### `tools.entries` — 工具定义

列出 Agent 可用的工具。如果数组为空，默认启用全部内置工具。

| 字段 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `name` | string | **是** | — | 工具标识符（完整描述参见 [docs/tools.md](tools.md)） |
| `enabled` | bool | 否 | `true` | 是否启用该工具 |

```json
{
  "tools": {
    "entries": [
      { "name": "shell", "enabled": true },
      { "name": "file_reader", "enabled": true },
      { "name": "file_editor", "enabled": false }
    ]
  }
}
```

#### `tools.approval` — 风险级的管控档

每个工具自带一个**不可配置**的风险级（见 `docs/tool.md` 的 `risk_level()`）；这里配置的是每个风险级受到多强的管控。三者关系与完整流转见 `docs/adr/0011-approval-by-turn-end-redemption.md`。

| 管控档 | 语义 |
|--------|------|
| `allow` | 直接执行 |
| `ask` | 中止本次调用、记录一条待确认、结束本回合（下一轮重放同一调用即兑现） |
| `deny` | 直接拒绝，返回固定拒绝文案，run 继续 |

字段：

| 字段 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `levels` | object | 否 | 见下 | 按风险级配置管控档，键为风险级名 |
| `tools` | object | 否 | `{}` | 按工具名覆盖某个工具的风险级管控档，优先于 `levels` |
| `timeout_seconds` | number | 否 | `1800` | 待确认记录的有效期（秒），夹取到 `1..=86400` |

`levels` 的键即风险级名：`read_only`、`workspace_write`、`exec`、`network_egress`、`external_send`。默认值：

```json
{
  "tools": {
    "entries": [{ "name": "shell", "enabled": true }],
    "approval": {
      "levels": {
        "read_only": "allow",
        "workspace_write": "allow",
        "exec": "ask",
        "external_send": "ask"
      },
      "tools": { "web_fetch": "deny" },
      "timeout_seconds": 1800
    }
  }
}
```

约定与边界：

- **不热重载**：策略在启动时读一次并快照进内存，改动需重启 gateway 才生效（待确认集合本身也只在内存里，重启即清空）。
- **按风险级配置而非逐个工具**：新出现的工具（例如 MCP 连接器带来的工具）自动继承所在风险级的策略，不会因为"忘了配"而静默放开。
- **无人值守按 `deny`**：cron / heartbeat / trigger / subagent 的 run 遇到 `ask` 一律拒绝，且不写待确认记录。
- 审批只约束 agent 主动发起的动作；Agent 写配置文件不走审批（见 ADR 0002、票 02）。

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

## 密钥与凭证（keychain → env → 配置文件）

provider 的 `api_key` 和 `triggers.secrets` 里的值都走同一条解析链，**顺序严格**：

1. **OS keychain**（macOS Keychain / Windows Credential Manager / Secret Service）
2. **环境变量**
3. **配置文件里的值**（最后回退，兼容旧明文）

先命中的非空值获胜：keychain 有值就压过环境变量，环境变量有值就压过配置文件。
反过来说，**配置文件里的值不会“赢过” keychain/env** —— 只要 keychain 或环境变量里
存在同名键，配置里写的那份就被忽略（不会被删除、也不会被改写，只是不生效）。
某个来源不可用（比如无头机器上没有 keychain）时静默回退到下一级，不会让启动失败。

### 键名

| 用途 | 键 | keychain（service / account） | 环境变量 |
|------|-----|------------------------------|----------|
| provider API key | `slimbot.<provider-name>` | service = 键，account = `slimbot` | `SLIMBOT_<PROVIDER-NAME>` |
| 触发器密钥 | `slimbot.trigger.<secret-name>` | service = 键，account = `slimbot` | `SLIMBOT_TRIGGER_<SECRET-NAME>` |

环境变量名由键推导：全部大写，非字母数字字符替换为 `_`。例如 provider `default` →
`slimbot.default` → `SLIMBOT_DEFAULT`；触发器密钥 `github-token` →
`slimbot.trigger.github-token` → `SLIMBOT_TRIGGER_GITHUB_TOKEN`。

### 写入 keychain

```bash
# macOS（service = 键，account 恒为 slimbot）
security add-generic-password -s slimbot.default -a slimbot -w 'sk-...'
security add-generic-password -s slimbot.trigger.github -a slimbot -w 'ghp_...'

# 删除
security delete-generic-password -s slimbot.default -a slimbot
```

Linux 的 Secret Service（`secret-tool`）与 Windows Credential Manager 需要先启用对应
crate 特性（见下方平台限制），v1 尚未启用。

> **平台限制（v1）**：`keyring` 只开了 `apple-native` 特性，因此 keychain 这一级只在
> macOS/iOS 真正生效；Linux / Windows 目前没有启用的原生后端（会退化为“无条目”），
> 密钥走环境变量或配置文件。补上 `windows-native` / `sync-secret-service` 特性是后续工作。

### 文件权限（0600）

- 含密钥值的 `config.json`（`api_key` 或 `triggers.secrets[].value` 非空）在**写出时**
  被设为 `0600`（仅 owner 可读写）；已存在的宽权限文件会在加载时被收紧。
- 不含密钥值的配置文件不动权限（你给的 mode 保持原样）。
- 权限收紧只操作 `config.json` 本身，不动目录；日志只记路径，不记内容。

### setup 与迁移

- `slimbot setup` 只用 `load_for_preview` 读盘，**不会**把 keychain/env 里的值拷进
  `config.json`：密钥留在它原本待的地方。
- 已有明文 `api_key` **不会**被自动删除或迁移到 keychain，也不会被自动清空。想迁走就
  自己写进 keychain 或环境变量，然后把字段留空（留空的字段在 WebUI 编辑器里等价于
  “不变”，需要清空时请直接改文件或经 `triggers.secrets` 删除条目）。
- 留空字段不会让 `slimbot setup` 失败（setup 不做 `api_key` 校验）；但正常启动会校验，
  解析链全部缺失时报错信息会把三条路径都列出来：

  ```text
  provider 'default'.api_key must not be empty. Set it in your config file,
  export SLIMBOT_DEFAULT, or store it in the OS keychain as `slimbot.default`.
  ```

### 脱敏

- 解析路径只在 debug 日志里记 **键 + 来源**（`resolved slimbot.default from keychain`），
  永不记值；值在内部以 `Secret` 包装，`Debug`/`Display` 恒为 `***`。
- WebUI 配置编辑器对 `secret` 字段的 value 恒为 `null`；`triggers` 段只声明
  `secrets` 集合的名字，条目的值只在专门的 `triggers.secrets` 段里以掩码形态出现。
- 触发器脚本读到值后由 `Redactor` 负责抹掉回显（见 [codemode.md](codemode.md)）。

## 验证规则

加载配置时强制执行以下验证：

- `agent.provider` 不能为空
- 引用的 Provider 必须存在于 `providers` 中
- 引用的 Provider 的 `api_key` 不能为空（keychain / 环境变量 / 配置文件全部缺失时才报错）
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
  - `api_key` **永不**被规范化或覆盖（解析链的覆盖是另一回事：它发生在 clamp/normalize 之后的解析步骤）
- `name` 为空的 tools 条目会被移除；channels 按空键名过滤

规范化在 clamp 之后执行，确保归一化过程产生的值也经过约束检查。

## 全局单例与热重载

配置通过全局单例访问：`Config::init(path)` 初始化，`Config::get()` 返回当前配置快照。

### 文件监听

初始化后自动启动文件系统监听（基于 `notify` crate，macOS 使用 FSEvents）。当 `config.json` 被修改时：

1. 重新读取文件，执行 clamp + normalize + 密钥解析 + validate
2. 计算新旧配置差异（diff），生成 `ConfigChange { paths, old_values, new_values }`
3. 通知匹配的订阅者（基于路径前缀匹配）

重新读取走的是与启动相同的解析入口，因此 keychain / 环境变量里的密钥在重载时也会
重新解析（keychain 里轮换了密钥，改一下 `config.json` 触发重载即可生效）。

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

## WebUI 配置编辑器

Gateway 模式的 WebUI 提供可视化配置编辑页面（入口 `/config`，从聊天页/查看器页顶部图标进入）。**表单由配置定义自动生成，前端不手写任何配置项**：

- **Schema 自动生成** — 后端把 `define_config!` 宏生成的字段元数据（`FieldMeta`）序列化为 config schema（`src/config_schema.rs::serialize_schema`），前端按 schema 的 `kind` 渲染控件（bool 开关 / int/float 数字框带 min/max / string 文本框带 maxlength / select 下拉 / secret 密码框 / optional_string / 集合编辑器）。新增/修改 Rust 配置字段后表单自动跟随。
- **集合编辑** — `providers`（键值映射，结构化增删改/重命名）、`tracing.backends`（数组）、`tools`（名称下拉 + enabled 开关）、`channels`（enabled 开关 + extra 裸 JSON）。
- **写入语义** — 提交时把脏字段编译为 `[{op, path, value}]`（`set`/`add`/`remove`，dot-path 如 `agent.max_iterations`、`providers.<name>.temperature`、`gateway.cron.enabled`），服务端读取当前 `config.json` → 应用 patch → 按与 `Config::load` 相同的顺序执行 clamp → normalize → validate → 写回。校验失败**不写盘**，返回逐字段错误供前端高亮。校验用的是“解析链之后”的副本（跟一次真实启动看到的一样），所以 `api_key` 存在 keychain / 环境变量里、文件里留空时，编辑其他字段不会被误报；但写回的仍是文件形态，解析出来的值不会被持久化。未知 JSON 字段在 patch 写入中保留。重命名 provider 时 `add` 操作携带 `from`（源 key），服务端从源条目继承掩码字段（如 `api_key`）与未知字段，避免重命名丢密钥。
- **密钥保护** — `api_key` 等 `secret` 字段在 schema 中 value 恒为 `null`，绝不回明文；编辑时留空 = 不变，填入新值即轮换密钥。`triggers` 段只列出 `secrets` 集合的名字（`[{name}]`），条目的值是 `secret` 字段，同样恒为 `null`。
- **重启生效** — 改动写回 `config.json` 后需重启 Gateway 才生效（v1 不做进程内热重载，见 `docs/adr/0002`）。页面顶部常驻提示。

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
