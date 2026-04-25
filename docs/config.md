# Configuration Guide

This document describes how to configure slimbot.

## Quick Start

Run the setup command to generate a default configuration file:

```bash
cargo run -- setup          # creates ~/.slimbot/config.json
cargo run -- setup /path/to/custom.json   # custom path
```

If the config file already exists, `setup` will load it, fill in any missing default values, normalize invalid entries, and write it back.

## Config File Location

| Method | Path |
|--------|------|
| Default (no argument) | `~/.slimbot/config.json` |
| CLI argument | `cargo run -- <config.json路径>` |
| Setup command | `cargo run -- setup [config.json路径]` |

## Data Directory

All runtime data is stored under `data_dir` (default: `~/.slimbot/`). The directory structure:

```
~/.slimbot/
├── config.json                     # Configuration file
└── workspace/
    ├── agent.md                    # Agent behavior definition
    ├── user.md                     # User profile
    ├── soul.md                     # Agent personality
    ├── tools.md                    # Tool usage guide
    ├── skills/                     # Optional skill files (*.md)
    └── sessions/
        └── {session_id}.jsonl      # Session message persistence
```

The `sessions/` directory is created automatically on startup.

## Configuration Structure

```json
{
  "data_dir": "~/.slimbot",
  "agent": { ... },
  "providers": { ... },
  "tools": [ ... ],
  "channels": [ ... ]
}
```

### Top-Level Fields

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `data_dir` | string | No | `~/.slimbot` | Base directory for all runtime data |
| `agent` | object | **Yes** | — | Single agent configuration |
| `providers` | object | **Yes** | — | Named provider definitions (keyed map) |
| `tools` | array | No | `[]` | Registered tool definitions |
| `channels` | array | No | `[]` | Communication channel definitions |

### `agent` — Agent Configuration

Slimbot has exactly one agent. The agent references a provider by name from the `providers` map.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `provider` | string | **Yes** | — | Name of the provider to use (must match a key in `providers`) |
| `max_iterations` | uint | No | `40` | Maximum tool-use iterations per turn |
| `timeout_seconds` | uint | No | `120` | Maximum time (seconds) per agent turn |

```json
{
  "agent": {
    "provider": "my_provider_a",
    "max_iterations": 20,
    "timeout_seconds": 60
  }
}
```

### `providers` — LLM Provider Definitions

A keyed map of named providers. Each provider can be referenced by the agent via its key name.

Provider-level fields:

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | No | `"openai"` | Provider type: `"openai"` or `"custom"` |
| `api_url` | string | No | see below | Full API endpoint URL |
| `base_url` | string | No | — | Base URL of the provider (e.g. `"https://api.openai.com"`) |
| `api_key` | string | **Yes** | — | API authentication key (validated on load) |
| `model` | string | **Yes** | — | Model name to use (validated on load) |
| `temperature` | float | No | `0.7` | Sampling temperature (0.0–2.0) |
| `max_tokens` | uint | No | `4096` | Maximum tokens per response |

**URL resolution logic:** `api_url` takes priority. If empty, `api_url` is derived from `base_url + "/v1/chat/completions"`. If neither is set, defaults to `https://api.openai.com/v1/chat/completions`.

**Provider types:**

- `"openai"` — Default. Uses OpenAI's API endpoint.
- `"custom"` — Any OpenAI-compatible API. Set `base_url` to your provider's root URL.

Example with multiple providers:

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

### `tools` — Tool Definitions

Lists tools available to the agent. Each tool can be individually enabled or disabled.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `name` | string | **Yes** | — | Tool identifier |
| `enabled` | bool | No | `true` | Whether the tool is active |

```json
{
  "tools": [
    { "name": "bash", "enabled": true },
    { "name": "file_editor", "enabled": false }
  ]
}
```

### `channels` — Communication Channels

Defines input/output channels for the agent. Each entry specifies a channel type and optional configuration.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | **Yes** | — | Channel type identifier (e.g. `"cli"`) |
| `enabled` | bool | No | `true` | Whether the channel is active |
| `config` | object | No | `{}` | Channel-specific configuration (arbitrary JSON) |

```json
{
  "channels": [
    { "type": "cli", "enabled": true, "config": {} }
  ]
}
```

## Minimal Config

Only `agent.provider`, the referenced provider's `api_key` and `model` are required:

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

## Validation Rules

On config load, the following validations are enforced:

- `agent.provider` must not be empty
- The referenced provider must exist in `providers`
- The referenced provider's `api_key` must not be empty
- The referenced provider's `model` must not be empty

If validation fails, the application will exit with an error message.

## Normalization

When running `cargo run -- setup` on an existing config, the following normalization rules apply:

- Empty `data_dir` → `~/.slimbot`
- Empty `agent.provider` → `"default"`
- Empty `agent.max_iterations` → `40`
- Empty `agent.timeout_seconds` → `120`
- For each provider:
  - Empty `type` → `"openai"`
  - Empty `api_url` with `base_url` set → `base_url + "/v1/chat/completions"`
  - Empty `api_url` and `base_url` → `https://api.openai.com/v1/chat/completions`
  - Empty `model` → `gpt-4o`
  - `temperature` ≤ 0.0 or > 2.0 → `0.7`
  - `max_tokens` = 0 → `4096`
  - `api_key` is **never** normalized or overwritten
- Tools/channels with empty `name`/`type` are removed

## Complete Example

```json
{
  "data_dir": "/Users/takethat/.slimbot",
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
    { "name": "bash", "enabled": true }
  ],
  "channels": [
    { "type": "cli", "enabled": true, "config": {} }
  ]
}
```

## Multiple Providers Example

Define multiple providers and switch which one the agent uses by changing `agent.provider`:

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
