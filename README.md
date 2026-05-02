# SlimBot

A lightweight AI chatbot assistant built in Rust, powered by the ReAct (Reason + Act) pattern and OpenAI-compatible LLM APIs. Supports tool calling and multi-channel I/O (CLI, WebUI, and more).

## Features

- **ReAct Loop** — Agent reasons and acts in cycles: builds context, calls the LLM, executes tool calls, and repeats until a final answer is reached.
- **Tool Calling** — Extensible tool system via the `Tool` trait with OpenAI function calling format conversion.
- **Multi-Channel** — Pluggable channel architecture via the `Channel` / `ChannelFactory` traits. Ships with a CLI channel.
- **Session Persistence** — JSONL-based session storage with automatic load/save.
- **Workspace-Driven Context** — System prompt assembled from workspace files (`agent.md`, `user.md`, `soul.md`, `tools.md`) and optional skill files (`skills/*.md`).
- **Configurable** — LLM provider, agent behavior, channels, and tools all driven by a single `config.json`.

## Quick Start

### Prerequisites

- Rust 2024 edition toolchain (`rustc`, `cargo`)

### Build

```bash
cargo build
```

### Run

```bash
# With explicit config path
cargo run -- /path/to/config.json

# Uses default ~/.slimbot/config.json if no argument given
cargo run
```

### Configuration

Create `~/.slimbot/config.json`:

```json
{
  "data_dir": "/home/user/.slimbot",
  "provider": {
    "api_url": "https://api.openai.com/v1/chat/completions",
    "api_key": "sk-...",
    "model": "gpt-4o",
    "temperature": 0.7,
    "max_tokens": 4096
  },
  "agent": {
    "max_iterations": 40,
    "timeout_seconds": 120
  },
  "tools": [
    { "name": "shell", "enabled": true }
  ],
  "channels": [
    {
      "type": "cli",
      "enabled": true,
      "config": { "prompt": "> " }
    }
  ]
}
```

### Workspace

The workspace directory (`~/.slimbot/workspace/`) contains files that shape the agent's behavior:

| File | Purpose |
|---|---|
| `agent.md` | Agent behavior definition |
| `user.md` | User profile |
| `soul.md` | Agent personality |
| `tools.md` | Tool usage guide |
| `skills/*.md` | Optional skill files |

Session data is stored in `~/.slimbot/workspace/sessions/{session_id}.jsonl`.

## Architecture

```
AgentLoop ── initializes Provider, ToolManager, SessionManager
   │
   ▼
AgentRunner ── ReAct Loop
   ├── ContextBuilder.build() → system prompt + history + tool definitions
   ├── Provider.chat()        → LLM API call
   ├── ToolManager.execute()  → tool execution
   └── SessionManager.persist() → JSONL persistence
   │
   ▼
MessageBus ── BusRequest → SessionTask → enqueue/dequeue → AgentRunner
   │
   ▼
ChannelManager ── create channels from config → poll input → interact with MessageBus
```

### Key Design Decisions

- **Shared access**: `SharedSessionManager = Arc<Mutex<SessionManager>>`, all modules access through a shared mutex.
- **Status events**: `SessionTask` carries a `TaskHook`, which sends state changes via `tokio::sync::mpsc` to the channel.
- **Persistence**: Full JSONL write at the end of each AgentRunner cycle (append, not overwrite).
- **Session ID format**: `{channel_id}:{chat_id}`
- **Loop termination**: exits when `max_iterations` is exceeded or the model responds without tool calls.

## CLI Arguments

```bash
slimbot [CONFIG]              # Positional config path (backward compatible)
slimbot -c, --config <PATH>   # Config file path
slimbot -d, --data-dir <PATH> # Data directory
slimbot -w, --workspace-dir <PATH>  # Workspace directory
slimbot --log <LEVEL>         # Log level: 0=debug, 1=info (default), 2=warning, 3=error, 4=fatal
slimbot --log-file <PATH>     # Also write logs to file (tilde expansion supported)
```

Subcommands:
- `setup` — Run setup wizard (create/normalize config, initialize directories)
- `agent [-s SESSION_ID]` — Start CLI interactive agent session

See [docs/logging.md](docs/logging.md) for detailed logging configuration.

## Project Structure

```
slimbot/
├── Cargo.toml
├── .gitignore
└── src/
    ├── main.rs         # Entry point: config → AgentLoop → MessageBus → ChannelManager
    ├── config.rs       # Config struct: JSON loading, ProviderConfig, AgentConfig, ChannelEntry
    ├── tool.rs         # Tool trait + ToolManager: registration & OpenAI function calling conversion
    ├── provider/       # LLM provider abstraction
    │   ├── mod.rs      # Provider trait, ChatResponse, FinishReason
    │   └── openai.rs   # OpenAIProvider implementation
    ├── session.rs      # Session + SessionManager: FIFO queue, message management, JSONL persistence
    ├── context.rs      # ContextBuilder: system prompt assembly from workspace files + skills + history
    ├── runner.rs       # AgentRunner: ReAct loop core
    ├── agent_loop.rs   # AgentLoop: top-level orchestration
    ├── message_bus.rs  # MessageBus: request routing and result delivery
    └── channel/        # I/O channel abstraction
        ├── mod.rs      # Channel/ChannelFactory traits, ChannelManager
        └── cli.rs      # CliChannel & CliChannelFactory
```

## Development

```bash
cargo check    # Quick compile check
cargo build    # Full build
cargo test     # Run tests
```

## License

MIT
