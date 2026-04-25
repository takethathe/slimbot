# Built-in Tools

SlimBot ships with 6 built-in tools for shell execution and file operations. All tools are enabled by default and can be individually disabled in `config.json`.

## Tool List

### `shell`

Execute arbitrary shell commands via `sh -c`.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `command` | string | Yes | The shell command to execute |

**Behavior:**
- Returns stdout and stderr (prefixed with `stdout:` / `stderr:` labels)
- Default timeout: 30 seconds. If the command exceeds this, an error is returned.
- Exit code is appended to the output if non-zero.

**Example:**
```json
{ "command": "ls -la /tmp" }
```

---

### `file_reader`

Read the contents of a file within the data directory.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `path` | string | Yes | File path (relative to data directory) |

**Behavior:**
- Output is truncated to 50,000 characters to prevent context explosion.
- Returns an error if the path is not a file or does not exist.

**Example:**
```json
{ "path": "workspace/agent.md" }
```

---

### `file_writer`

Write content to a file, creating it and any parent directories as needed.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `path` | string | Yes | File path (relative to data directory) |
| `content` | string | Yes | Content to write |

**Behavior:**
- Creates the file if it does not exist, or overwrites if it does.
- Automatically creates parent directories.

**Example:**
```json
{ "path": "workspace/notes.md", "content": "# Notes\n\nSome content here." }
```

---

### `file_editor`

Perform an exact search-and-replace in a file.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `path` | string | Yes | File path (relative to data directory) |
| `old_string` | string | Yes | The exact text to find (must appear **exactly once**) |
| `new_string` | string | Yes | The replacement text |

**Behavior:**
- Rejects the edit if `old_string` is not found or appears more than once.
- This ensures surgical precision — no accidental mass replacements.

**Example:**
```json
{
  "path": "workspace/agent.md",
  "old_string": "You are a helpful assistant.",
  "new_string": "You are a concise assistant."
}
```

---

### `list_dir`

List the contents of a directory within the data directory.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `path` | string | Yes | Directory path (relative to data directory) |

**Behavior:**
- Entries are sorted alphabetically.
- Each entry is prefixed with a type indicator:
  - `[D]` — Directory
  - `[F]` — File
  - `[L]` — Symlink or other

**Example:**
```json
{ "path": "workspace" }
```

---

### `make_dir`

Create a directory, including all parent directories.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `path` | string | Yes | Directory path (relative to data directory) |

**Behavior:**
- Equivalent to `mkdir -p`. No error if the directory already exists.

**Example:**
```json
{ "path": "workspace/skills/new-skill" }
```

## Security

All file operations are restricted to the `data_dir` configured in `config.json`.

- **Path validation**: Every file path is resolved against `data_dir` and validated using canonical absolute paths. Paths that escape the data directory (via `../`, symlinks, etc.) are rejected.
- **Shell isolation**: The `shell` tool has no directory restriction but is subject to a 30-second timeout.
- **Read size limit**: `file_reader` output is capped at 50,000 characters.

## Configuration

Tools are configured in `config.json` under the `tools` array. If the array is empty, all 6 built-in tools are enabled by default.

```json
{
  "tools": [
    { "name": "shell", "enabled": true },
    { "name": "file_reader", "enabled": true },
    { "name": "file_writer", "enabled": true },
    { "name": "file_editor", "enabled": true },
    { "name": "list_dir", "enabled": false },
    { "name": "make_dir", "enabled": true }
  ]
}
```

To disable a tool, set its `enabled` field to `false`, or remove the entry entirely.

## Adding Custom Tools

New tools can be added by implementing the `Tool` trait from `src/tool.rs`:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;
    async fn execute(&self, args: serde_json::Value) -> Result<String>;
}
```

1. Create a new module in `src/tools/` with the tool struct.
2. Register the tool in the `create_tool()` factory function in `src/tools/mod.rs`.
3. Optionally add the tool name to the default list in `ToolManager::init_from_config()`.

The `parameters()` method returns a JSON Schema object that describes the tool's arguments, which is converted to OpenAI function calling format.
