# SlimBot

一个用 Rust 编写的个人 AI 助手：基于 ReAct 循环，兼容 OpenAI 的 LLM API，支持工具调用、长期记忆、定时自动化与多通道 I/O（CLI、WebUI、飞书）。

## 特性

- **ReAct 循环** — 构建上下文 → 调用模型 → 执行工具 → 重复，直到模型给出不带工具调用的回答。
- **工具** — 10 个内置工具（shell、文件读写编辑、目录列表/创建、grep、find_files、web_fetch）加一个 `codemode` JavaScript 沙箱；连接器（MCP）工具只能在 codemode 脚本里访问。
- **审批** — 每个工具自述风险级，配置把风险级映射到 allow / ask / deny；`ask` 在下一轮兑现。
- **记忆** — 由受保护文档漏斗守住唯一可写文件 `MEMORY.md`，配派生索引、BM25-lite 召回，以及 Dream 定期整理历史。
- **自动化** — 定时任务（cron）、心跳检查，以及数据源触发器（采集 → 判定 → 投递）。
- **子代理** — 一次性委派运行，与触发器行动共用全局并发闸。
- **多通道** — CLI 与 WebUI（SSE），加一个带流式卡片的飞书 IM 通道。
- **会话持久化** — append-only JSONL，只有压缩后才会全量重写。
- **可配置** — 单一 `config.json`；密钥按 OS keychain → 环境变量 → 文件（`0600`）顺序解析。

## 快速开始

### 前置条件

- Rust 2024 edition 工具链（`rustc`、`cargo`）

### 构建

```bash
cargo build
```

### 运行

```bash
cargo run -- setup                     # 创建 ~/.slimbot/config.json 与工作区
cargo run -- agent                     # CLI 交互会话
cargo run -- agent "总结一下今天"        # 单轮跑完即退出
cargo run -- gateway                   # cron + heartbeat + 触发器 + 已启用通道（WebUI、飞书）
```

没有默认子命令：只跑 `cargo run` 会打印 help。路径由 `-c/--config`、`-d/--data-dir`（默认 `~/.slimbot`）和 `-w/--workspace-dir`（默认 `{data_dir}/workspace`）决定。

### 配置

`setup` 写出所有字段都取默认值的配置文件。需要你填的只有 provider 的 key 和一个通道：

```json
{
  "agent": { "provider": "default" },
  "providers": {
    "default": { "api_key": "sk-...", "model": "gpt-4o" }
  },
  "channels": {
    "webui": { "enabled": true }
  }
}
```

`api_key` 留空时，只要能作为环境变量 `SLIMBOT_DEFAULT` 或 OS keychain 条目 `slimbot.default` 取到即可。完整字段（每个 provider 的 context window、上下文压力占比、MCP server 等）见 [docs/config.md](docs/config.md)。

### 工作区

`~/.slimbot/workspace/` 存放决定 agent 行为的东西：

| 路径 | 用途 |
|---|---|
| `AGENTS.md` | Agent 运行指令 |
| `USER.md` | 用户画像 |
| `SOUL.md` | Agent 人格 |
| `TOOLS.md` | 工具使用说明 |
| `memory/MEMORY.md` | 长期记忆（唯一被直接写入的记忆文档） |
| `agents/` | 子代理定义（默认铺出 `trigger.md` 与 `executor.md`） |
| `skills/` | 可加载的技能文档 |
| `references/` | 两个子代理共享的文档（连接器契约）；不做成 skill |
| `triggers/` | 触发器源与规则，以及触发历史 |
| `executors/` | 执行器定义 `exec-xxxx/{meta.json, act.js}`——存好的动作，按需跑一次 |
| `sessions/` | `{session_id}.jsonl` 会话日志 |
| `cron/jobs.json` | 定时任务 |

仍在模板原文状态的 bootstrap 文件不会注入 system prompt。

## 架构

```
Channel → MessageBus → AgentLoop → SessionRunner → AgentRunner (ReAct)
                                                     ├── ContextBuilder   system prompt + 历史 + 工具 schema
                                                     ├── Provider.chat()  调用 LLM
                                                     ├── ToolManager      执行工具（经审批闸）
                                                     └── SessionManager   写入 JSONL
                                     outbound → ChannelManager → Channel
```

Gateway 模式复用同一套 `AgentLoop`，额外起 `CronService`、`HeartbeatService` 和 `TriggerService` + `Dispatcher`（tick 只采集/判定/入队，行动在队列外执行）。WebUI 通过 SSE 广播 `AgentEvent`。

### 关键设计决策

- **共享会话状态** — `SharedSessionManager = Arc<Mutex<SessionManager>>`。
- **Session ID** — `{channel_id}:{chat_id}`。
- **持久化** — append-only JSONL；只有压缩后才全量重写。
- **循环终止** — 超出 `max_iterations`，或模型返回无 tool_calls。
- **一个能力一个入口** — agent 是可被启动的执行体，skill 是可被加载的知识；同一能力只做其中一个。
- **外部内容只作证据** — 子代理与触发器的输入不会以原文进上下文，也不会进记忆。
- **连接器工具不进模型 schema** — 只在 codemode 里通过 `searchTools()` 发现。

每条背后的推理记录在 [docs/adr/](docs/adr/)。

## 命令行

```bash
slimbot [OPTIONS] [CONFIG] [COMMAND]

Commands:
  setup              创建或规范化配置，初始化目录
  agent [QUERY]      CLI 会话；带 QUERY 则跑一轮后退出
  gateway            cron + heartbeat + 触发器 + 已启用通道
```

| 参数 | 默认值 | 说明 |
|---|---|---|
| `-c, --config <PATH>` | `{data_dir}/config.json` | 配置文件（顶层选项，放在子命令之前） |
| `-d, --data-dir <PATH>` | `~/.slimbot` | 数据目录 |
| `-w, --workspace-dir <PATH>` | `{data_dir}/workspace` | 工作区目录（必须在数据目录之下） |
| `--log <LEVEL>` | `1` | `0`=debug `1`=info `2`=warning `3`=error `4`=fatal |
| `--log-file <PATH>` | — | 同时把日志写进文件 |

位置参数 `CONFIG` 仍可用，但必须配合子命令（`slimbot /path/config.json agent`）。日志配置见 [docs/logging.md](docs/logging.md)。

## 项目结构

```
slimbot/
├── src/
│   ├── main.rs, cli.rs, setup.rs, gateway.rs   入口、参数、setup、gateway
│   ├── agent_loop.rs, runner/                  顶层编排、ReAct 循环、审批 hook
│   ├── context.rs, session.rs, message_bus.rs  提示词组装、会话、请求路由
│   ├── consolidate.rs, snip.rs, pressure.rs    上下文预算：摘要、裁剪、长度限制
│   ├── provider/, channel/, tools/, tool.rs    LLM provider、I/O 通道、工具
│   ├── trigger/, cron/, heartbeat/, agent/     自动化、子代理运行时
│   ├── codemode/, mcp/, executor/              脚本沙箱、连接器池、执行器
│   ├── memory/, card/, approval/, trace/       记忆、卡片、审批、trace
│   └── config*.rs, log.rs, path.rs, secrets.rs, utils/
├── docs/          模块文档 —— 索引见 docs/index.md
├── tests/         集成测试
├── GLOSSARY.md    领域术语表
└── AGENTS.md      开发规范（指向 CLAUDE.md 的符号链接）
```

## 开发

```bash
cargo fmt -- --check
cargo clippy
cargo test
```

开发规范见 [AGENTS.md](AGENTS.md)，模块文档索引见 [docs/index.md](docs/index.md)。

## License

MIT