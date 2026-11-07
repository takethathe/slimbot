# SlimBot

A personal AI assistant written in Rust: a ReAct loop over OpenAI-compatible LLM APIs, with tool calling, long-term memory, scheduled automation and multi-channel I/O (CLI, WebUI, Feishu).

## Features

- **ReAct Loop** — builds context, calls the model, executes tool calls, repeats until the model answers without one.
- **Tools** — 10 built-in tools (shell, file read/write/edit, list/make dir, grep, find_files, web_fetch) plus a `codemode` JavaScript sandbox; connector (MCP) tools are reachable only from inside codemode scripts.
- **Approval** — every tool declares a risk level; a config policy maps risk to allow / ask / deny, and `ask` is redeemed on the next turn.
- **Memory** — one writable `MEMORY.md` behind a protected-document funnel, with a derived index, BM25-lite recall, and a Dream pass that consolidates history.
- **Automation** — cron jobs, a heartbeat check, and data-source triggers (collect → judge → dispatch).
- **Subagents** — one-shot delegated runs sharing a global concurrency gate with trigger actions.
- **Multi-Channel** — CLI and WebUI (SSE) plus a Feishu IM channel with streaming cards.
- **Session Persistence** — append-only JSONL, rewritten in full only after consolidation.
- **Configurable** — one `config.json`; secrets resolve from OS keychain → environment → file (`0600`).

## Quick Start

### Prerequisites

- Rust with the 2024 edition (`rustc`, `cargo`)

### Build

```bash
cargo build
```

### Run

```bash
cargo run -- setup                     # create ~/.slimbot/config.json and the workspace
cargo run -- agent                     # interactive CLI session
cargo run -- agent "summarize today"   # one turn, then exit
cargo run -- gateway                   # cron + heartbeat + triggers + enabled channels (WebUI, Feishu)
```

There is no default subcommand: `cargo run` on its own prints help. Paths come from `-c/--config`, `-d/--data-dir` (default `~/.slimbot`) and `-w/--workspace-dir` (default `{data_dir}/workspace`).

### Configuration

`setup` writes a config file with every field at its default. The two things you must fill in are a provider key and at least one channel:

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

`api_key` may stay empty when it is available as `SLIMBOT_DEFAULT` or as the OS-keychain entry `slimbot.default`. See [docs/config.md](docs/config.md) for the full schema (per-provider context window, pressure ratios, MCP servers, …).

### Workspace

`~/.slimbot/workspace/` holds everything that shapes the agent:

| Path | Purpose |
|---|---|
| `AGENTS.md` | Agent operating instructions |
| `USER.md` | User profile |
| `SOUL.md` | Agent personality |
| `TOOLS.md` | Tool usage notes |
| `memory/MEMORY.md` | Long-term memory (the only memory document written directly) |
| `agents/` | Subagent definitions (`trigger.md` ships by default) |
| `skills/` | Loadable skill documents |
| `triggers/` | Trigger sources and rules, plus fire history |
| `sessions/` | `{session_id}.jsonl` conversation logs |
| `cron/jobs.json` | Scheduled jobs |

Bootstrap files still holding their template text are not injected into the system prompt.

## Architecture

```
Channel → MessageBus → AgentLoop → SessionRunner → AgentRunner (ReAct)
                                                     ├── ContextBuilder   system prompt + history + tool schemas
                                                     ├── Provider.chat()  LLM call
                                                     ├── ToolManager      tool execution, gated by approval
                                                     └── SessionManager   JSONL persistence
                                     outbound → ChannelManager → Channel
```

Gateway mode runs the same `AgentLoop` and adds `CronService`, `HeartbeatService`, and `TriggerService` + `Dispatcher` (a tick only collects, judges and enqueues; actions run outside it). WebUI broadcasts `AgentEvent` over SSE.

### Key Design Decisions

- **Shared session state** — `SharedSessionManager = Arc<Mutex<SessionManager>>`.
- **Session id** — `{channel_id}:{chat_id}`.
- **Persistence** — append-only JSONL; a full rewrite happens only after consolidation.
- **Loop termination** — `max_iterations` exceeded, or a response without tool calls.
- **One entry per capability** — an agent is an executable body, a skill is loadable knowledge; never both for the same capability.
- **External content stays evidence** — subagent and trigger input never enters the context as raw text, and never enters memory.
- **Connector tools never enter the model schema** — they are discovered inside codemode via `searchTools()`.

The reasoning behind each is recorded in [docs/adr/](docs/adr/).

## CLI

```bash
slimbot [OPTIONS] [CONFIG] [COMMAND]

Commands:
  setup              Create or normalize the config and initialize directories
  agent [QUERY]      CLI session; with QUERY, run one turn and exit
  gateway            Cron + heartbeat + triggers + enabled channels
```

| Option | Default | Meaning |
|---|---|---|
| `-c, --config <PATH>` | `{data_dir}/config.json` | Config file (top-level option, before the subcommand) |
| `-d, --data-dir <PATH>` | `~/.slimbot` | Data directory |
| `-w, --workspace-dir <PATH>` | `{data_dir}/workspace` | Workspace directory (must sit under the data directory) |
| `--log <LEVEL>` | `1` | `0`=debug `1`=info `2`=warning `3`=error `4`=fatal |
| `--log-file <PATH>` | — | Also write logs to a file |

A positional `CONFIG` still works, but only together with a subcommand (`slimbot /path/config.json agent`). See [docs/logging.md](docs/logging.md) for log configuration.

## Project Structure

```
slimbot/
├── src/
│   ├── main.rs, cli.rs, setup.rs, gateway.rs   entry, arguments, setup, gateway
│   ├── agent_loop.rs, runner/                  orchestration, ReAct loop, approval hooks
│   ├── context.rs, session.rs, message_bus.rs  prompt assembly, sessions, request routing
│   ├── consolidate.rs, snip.rs, pressure.rs    context budget: summary, trimming, limits
│   ├── provider/, channel/, tools/, tool.rs    LLM providers, I/O channels, tools
│   ├── trigger/, cron/, heartbeat/, agent/     automation, subagent runtime
│   ├── codemode/, mcp/                         script sandbox, connector pool
│   ├── memory/, card/, approval/, trace/       memory, cards, approval, tracing
│   └── config*.rs, log.rs, path.rs, secrets.rs, utils/
├── docs/          module docs — index at docs/index.md
├── tests/         integration tests
├── GLOSSARY.md    domain vocabulary
└── AGENTS.md      development conventions (symlink to CLAUDE.md)
```

## Development

```bash
cargo fmt -- --check
cargo clippy
cargo test
```

Conventions live in [AGENTS.md](AGENTS.md); per-module documentation is indexed in [docs/index.md](docs/index.md).

## License

MIT