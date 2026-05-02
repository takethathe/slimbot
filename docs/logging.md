# Logging System

SlimBot uses a custom global singleton logger with level-based filtering and dual output (stderr + optional file).

## Log Levels

| Level | Value | Tag | Color (terminal) |
|-------|-------|-----|-------------------|
| Debug | 0 | `[D]` | White (default) |
| Info | 1 | `[I]` | Green |
| Warning | 2 | `[W]` | Yellow |
| Error | 3 | `[E]` | Red |
| Fatal | 4 | `[F]` | Orange |

## Usage

### Setting the Log Level

Use the `--log` flag with a numeric value. The logger outputs messages at that level **and above**:

```bash
# Debug: show all messages
slimbot --log=0 setup

# Info: show info, warning, error, fatal (default)
slimbot --log=1 setup

# Warning: suppress debug and info
slimbot --log=2 setup

# Error: only errors and fatal
slimbot --log=3 setup
```

### Writing to a File

Use `--log-file` to additionally write all log output to a file. The file uses plain text (no ANSI color codes):

```bash
slimbot --log=0 --log-file=/tmp/slimbot.log setup
```

Tilde expansion is supported in file paths: `--log-file=~/logs/slimbot.log`.

## Log Format

```
[2026-05-02 14:30:00] [I] message text here
```

- Terminal: colored level tag (e.g., green `[I]`)
- File: plain text level tag (e.g., `[I]`)

## Macros

Use the logging macros throughout the codebase:

```rust
use crate::{debug, info, warn, error, fatal};

debug!("connection established to {:?}", addr);
info!("session {} created", session_id);
warn!("retrying after {} failed attempts", retries);
error!("failed to load config: {}", e);
fatal!("critical: database corruption detected");
```

The `debug!` and `info!` macros include a `should_log()` check for efficient short-circuiting at compile time. `fatal!` also calls `std::process::exit(1)`.

## Architecture

- **Singleton**: `OnceLock<Arc<SharedLogger>>` ensures one global logger instance.
- **Thread-safe file I/O**: `Mutex<Option<File>>` protects the optional log file.
- **Level gating**: `should_log()` helper function allows macros to skip calls below the threshold without accessing internal state.
