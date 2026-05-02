# Channel Manager & Message Bus Refactor

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restructure ChannelManager and MessageBus so that MessageBus uses async channels for inbound/outbound communication, ChannelManager auto-initializes channels from config and acts as an outbound message router, and each channel manages its own client I/O thread.

**Architecture:** MessageBus splits into inbound (mpsc<BusRequest>) and outbound (mpsc<BusResult>) async channels. A background task drains inbound, runs AgentLoop, publishes results to outbound. ChannelManager holds channels and factories, listens on outbound, routes to target channel by channel_id. Each channel spawns its own tokio task for reading client input, forwarding to ChannelManager via inbound relay. Status updates via TaskHook remain unchanged.

**Tech Stack:** Rust, tokio, async_trait, serde

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `src/message_bus.rs` | **Rewrite** | Async inbound/outbound channels, background drain task |
| `src/channel/mod.rs` | **Rewrite** | ChannelManager: auto-init from config, outbound routing loop, `send_inbound()` relay |
| `src/channel/cli.rs` | **Modify** | Implement `start()` for internal read thread; `send_output()` for CLI (direct println) |
| `src/main.rs` | **Modify** | Simplify: no `channel_manager.run()`, use `channel_manager.start()` |

---

### Task 1: MessageBus — Async Inbound/Outbound Channels

**Files:**
- Modify: `src/message_bus.rs`
- Modify: `src/channel/mod.rs` (update import references)

- [ ] **Step 1: Rewrite message_bus.rs with async channels**

Replace the synchronous `send` method with an async queue-based architecture:

```rust
use std::sync::Arc;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::agent_loop::AgentLoop;
use crate::session::{SessionManager, SessionTask, TaskHook, TaskState, ensure_session};

pub struct BusRequest {
    pub session_id: String,
    pub content: String,
    pub channel_inject: Option<String>,
    pub hook: TaskHook,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusResult {
    pub session_id: String,
    pub task_id: String,
    pub content: String,
}

pub struct MessageBus {
    agent_loop: Arc<AgentLoop>,
    inbound_tx: mpsc::Sender<BusRequest>,
    outbound_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<BusResult>>>,
}

impl MessageBus {
    pub fn new(agent_loop: Arc<AgentLoop>) -> Self {
        let (inbound_tx, mut inbound_rx) = mpsc::channel::<BusRequest>(32);
        let (outbound_tx, outbound_rx) = mpsc::channel::<BusResult>(32);

        // Background task: drain inbound, run tasks, publish outbound
        let al = agent_loop.clone();
        let ot = outbound_tx.clone();
        tokio::spawn(async move {
            while let Some(request) = inbound_rx.recv().await {
                let sm = al.session_manager();
                ensure_session(&sm, &request.session_id).await.unwrap_or_else(|e| {
                    eprintln!("[MessageBus] Session error: {}", e);
                    continue;
                });

                let task_id = uuid::Uuid::new_v4().to_string();
                let mut task = SessionTask {
                    id: task_id.clone(),
                    content: request.content,
                    hook: request.hook,
                    state: TaskState::Pending,
                };

                // Enqueue
                {
                    let mut guard = sm.lock().await;
                    guard.enqueue_task(&request.session_id, task).await;
                }

                // Dequeue
                let mut task = {
                    let mut guard = sm.lock().await;
                    guard.dequeue_task(&request.session_id).await
                }
                .unwrap_or_else(|| {
                    SessionTask {
                        id: task_id.clone(),
                        content: String::new(),
                        hook: request.hook.clone(),
                        state: TaskState::Failed { error: "Queue empty".to_string() },
                    }
                });

                // Execute
                let agent_result = al
                    .run_task(&request.session_id, &mut task, request.channel_inject)
                    .await;

                let content = if agent_result.success {
                    agent_result.content
                } else {
                    format!("Error: {}", agent_result.content)
                };

                let _ = ot
                    .send(BusResult {
                        session_id: request.session_id,
                        task_id: task.id,
                        content,
                    })
                    .await;
            }
        });

        Self {
            agent_loop,
            inbound_tx,
            outbound_rx: Arc::new(tokio::sync::Mutex::new(outbound_rx)),
        }
    }

    /// Submit an inbound request (channel → agent)
    pub async fn send_inbound(&self, request: BusRequest) -> Result<()> {
        self.inbound_tx
            .send(request)
            .await
            .map_err(|_| anyhow!("MessageBus inbound channel closed"))
    }

    /// Get the outbound receiver for ChannelManager to subscribe to
    pub fn outbound_rx(&self) -> Arc<tokio::sync::Mutex<mpsc::Receiver<BusResult>>> {
        self.outbound_rx.clone()
    }
}
```

- [ ] **Step 2: Verify compilation**

Run: `cargo check`
Expected: May have errors in channel/mod.rs and main.rs due to changed MessageBus API — these will be fixed in subsequent tasks.

- [ ] **Step 3: Commit**

```bash
git add src/message_bus.rs
git commit -m "refactor: split MessageBus into async inbound/outbound channels with background drain task"
```

---

### Task 2: ChannelManager — Auto-init & Outbound Routing

**Files:**
- Modify: `src/channel/mod.rs`

- [ ] **Step 1: Rewrite ChannelManager**

Replace the existing ChannelManager with config-driven initialization and outbound routing:

```rust
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use super::{Channel, ChannelFactory};
use crate::message_bus::{BusRequest, BusResult, MessageBus};

pub struct ChannelManager {
    channels: HashMap<String, Box<dyn Channel>>,
    factories: HashMap<String, Box<dyn ChannelFactory>>,
    message_bus: Arc<MessageBus>,
}

impl ChannelManager {
    pub fn new(message_bus: Arc<MessageBus>) -> Self {
        Self {
            channels: HashMap::new(),
            factories: HashMap::new(),
            message_bus,
        }
    }

    /// Register a factory for a channel type
    pub fn register_factory(&mut self, type_name: &str, factory: Box<dyn ChannelFactory>) {
        self.factories.insert(type_name.to_string(), factory);
    }

    /// Initialize channels from config entries
    pub fn init_from_config(
        &mut self,
        entries: &[crate::config::ChannelEntry],
    ) -> Result<()> {
        for entry in entries {
            if !entry.enabled {
                continue;
            }
            let factory = self
                .factories
                .get(&entry.r#type)
                .ok_or_else(|| anyhow::anyhow!("Unregistered channel type: {}", entry.r#type))?;
            let channel = factory.create(&entry.config)?;
            let id = channel.id().to_string();
            eprintln!("Registered channel: {} ({})", channel.name(), channel.session_id());

            // Start the channel's internal read loop
            channel.start(self.message_bus.clone());

            self.channels.insert(id, channel);
        }
        Ok(())
    }

    /// Start the outbound routing loop
    pub async fn start(&mut self) -> Result<()> {
        let outbound_rx = self.message_bus.outbound_rx();
        let mut channels = std::mem::take(&mut self.channels);

        tokio::spawn(async move {
            let mut rx_guard = outbound_rx.lock().await;
            while let Some(result) = rx_guard.recv().await {
                // Extract channel_id from session_id (format: channel_id:chat_id)
                let channel_id = result
                    .session_id
                    .split(':')
                    .next()
                    .unwrap_or("");

                if let Some(channel) = channels.get_mut(channel_id) {
                    if let Err(e) = channel.send_output(&result).await {
                        eprintln!("[ChannelManager] Failed to send output to {}: {}", channel_id, e);
                    }
                } else {
                    eprintln!("[ChannelManager] No channel found for id: {}", channel_id);
                }
            }
        });

        // Restore channels back (they were moved into the spawned task's capture)
        // Actually, we need a different approach: Arc<Mutex<HashMap>> or don't take ownership
        Ok(())
    }
}
```

Wait — there's a problem. The spawned task needs mutable access to channels, but the `start` method returns immediately. Let me reconsider.

The outbound listener needs to hold mutable references to channels. We should use `Arc<Mutex<HashMap<String, Box<dyn Channel>>>>` or pass a shared reference. Let me revise:

```rust
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use anyhow::Result;

use super::{Channel, ChannelFactory};
use crate::message_bus::{BusRequest, BusResult, MessageBus};

pub struct ChannelManager {
    channels: Arc<Mutex<HashMap<String, Box<dyn Channel>>>>,
    factories: HashMap<String, Box<dyn ChannelFactory>>,
    message_bus: Arc<MessageBus>,
}

impl ChannelManager {
    pub fn new(message_bus: Arc<MessageBus>) -> Self {
        Self {
            channels: Arc::new(Mutex::new(HashMap::new())),
            factories: HashMap::new(),
            message_bus,
        }
    }

    /// Register a factory for a channel type
    pub fn register_factory(&mut self, type_name: &str, factory: Box<dyn ChannelFactory>) {
        self.factories.insert(type_name.to_string(), factory);
    }

    /// Initialize channels from config entries
    pub fn init_from_config(
        &mut self,
        entries: &[crate::config::ChannelEntry],
    ) -> Result<()> {
        let channels = self.channels.clone();
        let message_bus = self.message_bus.clone();

        for entry in entries {
            if !entry.enabled {
                continue;
            }
            let factory = self
                .factories
                .get(&entry.r#type)
                .ok_or_else(|| anyhow::anyhow!("Unregistered channel type: {}", entry.r#type))?;
            let channel = factory.create(&entry.config)?;
            let id = channel.id().to_string();
            let session_id = channel.session_id();
            let name = channel.name().to_string();

            eprintln!("Registered channel: {} ({})", name, session_id);

            // Start the channel's internal read loop
            let ch = channel;
            ch.start(message_bus.clone());

            channels.blocking_write().insert(id, ch);
        }
        Ok(())
    }

    /// Start the outbound routing loop
    pub fn start(&self) {
        let channels = self.channels.clone();
        let outbound_rx = self.message_bus.outbound_rx();

        tokio::spawn(async move {
            let mut rx_guard = outbound_rx.lock().await;
            while let Some(result) = rx_guard.recv().await {
                let channel_id = result
                    .session_id
                    .split(':')
                    .next()
                    .unwrap_or("");

                let mut ch_guard = channels.lock().await;
                if let Some(channel) = ch_guard.get_mut(channel_id) {
                    if let Err(e) = channel.send_output(&result).await {
                        eprintln!("[ChannelManager] Failed to send output to {}: {}", channel_id, e);
                    }
                } else {
                    eprintln!("[ChannelManager] No channel found for id: {}", channel_id);
                }
            }
        });
    }

    /// Submit an inbound request to MessageBus (called by channels)
    pub async fn send_inbound(&self, request: BusRequest) -> Result<()> {
        self.message_bus.send_inbound(request).await
    }
}
```

Also remove the `run` method and the old `init_from_config` that took factories as a parameter.

- [ ] **Step 2: Update Channel trait to add `start` and `send_output`**

Add two new methods to the Channel trait:

```rust
#[async_trait]
pub trait Channel: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn chat_id(&self) -> &str;
    async fn read_input(&mut self) -> Result<String>;
    async fn write_output(&mut self, result: &BusResult) -> Result<()>;
    async fn write_status(&mut self, session_id: &str, state: &TaskState) -> Result<()>;
    async fn prepare_inject(&self) -> Result<String>;
    fn session_id(&self) -> String {
        format!("{}:{}", self.id(), self.chat_id())
    }

    // New methods:
    /// Start the channel's internal client read loop. Called by ChannelManager after creation.
    fn start(self: Box<Self>, message_bus: Arc<MessageBus>) {
        // Default: spawn a task that reads input and forwards to ChannelManager
        // Implementations should override this.
    }

    /// Send output to client. Called by ChannelManager's outbound router.
    async fn send_output(&mut self, result: &BusResult) -> Result<()> {
        self.write_output(result).await
    }
}
```

The default `send_output` simply delegates to `write_output`. CLI channels use this default (direct println). Other channel types can override if they need different output routing.

The `start` method is also given a default implementation but will be overridden by concrete types.

- [ ] **Step 3: Remove old ChannelManager code**

Remove from `src/channel/mod.rs`:
- The old `channels: Vec<Box<dyn Channel>>` field
- The old `init_from_config` that takes `&[Box<dyn ChannelFactory>]`
- The old `register` method
- The old `run` method

- [ ] **Step 4: Update imports**

The Channel trait now needs to reference `MessageBus`. Add:
```rust
use crate::message_bus::{BusRequest, BusResult, MessageBus};
```

- [ ] **Step 5: Verify compilation**

Run: `cargo check`
Expected: Errors in cli.rs (missing `start` implementation) and main.rs (old API calls).

- [ ] **Step 6: Commit**

```bash
git add src/channel/mod.rs
git commit -m "refactor: ChannelManager auto-inits from config, routes outbound messages, channels self-manage I/O"
```

---

### Task 3: CliChannel — Internal Read Thread

**Files:**
- Modify: `src/channel/cli.rs`

- [ ] **Step 1: Add `start` implementation to CliChannel**

The CliChannel spawns a tokio task that reads from stdin and forwards to ChannelManager:

```rust
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use super::{Channel, ChannelFactory};
use crate::message_bus::{BusRequest, BusResult, MessageBus};
use crate::session::TaskHook;

pub struct CliChannel {
    channel_id: String,
    chat_id: String,
    prompt: String,
}

impl CliChannel {
    pub fn from_config(config: &serde_json::Value) -> Result<Self> {
        let prompt = config
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("> ")
            .to_string();
        Ok(Self {
            channel_id: "cli".to_string(),
            chat_id: "default".to_string(),
            prompt,
        })
    }
}

pub struct CliChannelFactory;

impl ChannelFactory for CliChannelFactory {
    fn channel_type(&self) -> &str {
        "cli"
    }

    fn create(&self, config: &serde_json::Value) -> Result<Box<dyn Channel>> {
        Ok(Box::new(CliChannel::from_config(config)?))
    }
}

#[async_trait]
impl Channel for CliChannel {
    fn id(&self) -> &str {
        &self.channel_id
    }

    fn name(&self) -> &str {
        "CLI"
    }

    fn chat_id(&self) -> &str {
        &self.chat_id
    }

    async fn read_input(&mut self) -> Result<String> {
        print!("{}", self.prompt);
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim().to_string();
        if input.is_empty() {
            return Err(anyhow::anyhow!("Input cannot be empty"));
        }
        Ok(input)
    }

    async fn write_output(&mut self, result: &BusResult) -> Result<()> {
        println!("\n{}\n", "-".repeat(40));
        println!("{}", result.content);
        println!("{}", "-".repeat(40));
        Ok(())
    }

    async fn write_status(&mut self, _session_id: &str, state: &TaskState) -> Result<()> {
        match state {
            TaskState::Running { current_iteration } => {
                eprintln!("  [Running] iteration {}", current_iteration);
            }
            TaskState::Completed { .. } => {
                eprintln!("  [Completed]");
            }
            TaskState::Failed { error } => {
                eprintln!("  [Failed] {}", error);
            }
            TaskState::Pending => {}
        }
        Ok(())
    }

    async fn prepare_inject(&self) -> Result<String> {
        Ok(String::new())
    }

    fn start(self: Box<Self>, message_bus: Arc<MessageBus>) {
        let session_id = self.session_id();
        let channel_name = self.name().to_string();
        let hook = TaskHook::new(&session_id);

        // Use spawn_blocking for blocking stdin read
        tokio::task::spawn_blocking(move || {
            let mut channel = self;

            loop {
                // Blocking read from stdin
                print!("{}", channel.prompt);
                let _ = std::io::stdout().flush();
                let mut input = String::new();
                if std::io::stdin().read_line(&mut input).is_err() {
                    eprintln!("[{}] Read failed", channel_name);
                    continue;
                }

                let input = input.trim().to_string();
                if input.is_empty() {
                    continue;
                }

                let request = BusRequest {
                    session_id: session_id.clone(),
                    content: input,
                    channel_inject: None,
                    hook: hook.clone(),
                };

                // Use blocking_send since we're in a spawn_blocking context
                // We need to enter the tokio runtime to send
                let rt = tokio::runtime::Handle::current();
                let mb = message_bus.clone();
                rt.block_on(async {
                    if let Err(e) = mb.send_inbound(request).await {
                        eprintln!("[{}] Failed to send inbound: {}", channel_name, e);
                    }
                });
            }
        });
    }
}
```

Key decisions:
- `spawn_blocking` is used because `stdin.read_line()` is a blocking operation
- We create our own `TaskHook` since the channel owns its session
- The `start` method takes `self: Box<Self>` so ownership moves into the spawned task
- Output (`write_output`) is called directly from the outbound routing task — no separate write queue needed since CLI output is thread-safe

- [ ] **Step 2: Verify compilation**

Run: `cargo check`
Expected: May have errors in main.rs due to old API calls.

- [ ] **Step 3: Commit**

```bash
git add src/channel/cli.rs
git commit -m "feat: CliChannel spawns internal read loop via spawn_blocking, forwards to ChannelManager"
```

---

### Task 4: main.rs — Simplify Startup

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Update main.rs**

Replace the old startup sequence:

```rust
#[tokio::main]
async fn main() -> Result<()> {
    let args = CliArgs::parse();

    if let Some(Commands::Setup { config }) = &args.command {
        let config_path = config
            .as_ref()
            .map(|p| p.to_str().unwrap())
            .or_else(|| args.config_path());
        let data_dir = args.data_dir().unwrap_or("~/.slimbot");
        return setup::run_setup(config_path, data_dir, args.workspace_dir());
    }

    let paths = PathManager::resolve(
        args.config_path(),
        args.data_dir(),
        args.workspace_dir(),
    )?;

    eprintln!("SlimBot starting... config file: {}", paths.config_path().display());

    let agent_loop = AgentLoop::from_config(&paths).await?;
    let agent_loop = Arc::new(agent_loop);

    let message_bus = Arc::new(MessageBus::new(agent_loop));

    let mut channel_manager = ChannelManager::new(message_bus);
    channel_manager.register_factory("cli", Box::new(CliChannelFactory));

    let config = crate::config::Config::load(paths.config_path().to_str().unwrap())?;
    channel_manager.init_from_config(&config.channels)?;

    channel_manager.start();

    // Keep the main task alive — channels and message bus run in background tasks
    std::future::pending::<()>().await;

    Ok(())
}
```

Key changes:
- Removed `channel_manager.run().await?` (the blocking loop)
- Added `channel_manager.start()` (spawns outbound routing in background)
- Added `std::future::pending::<()>().await` to keep the main task alive

- [ ] **Step 2: Verify compilation**

Run: `cargo check`
Expected: PASS (all compilation errors should be resolved)

- [ ] **Step 3: Run full build**

Run: `cargo build`
Expected: PASS

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: All existing tests pass

- [ ] **Step 5: Manual smoke test**

Run: `cargo run -- setup` (if not already done)
Then: `cargo run --` (with default config)
Expected: CLI prompt appears, can type a message, agent responds

- [ ] **Step 6: Commit**

```bash
git add src/main.rs
git commit -m "refactor: simplify main.rs — ChannelManager self-drives, main task awaits pending"
```

---

## Dependencies Between Tasks

Task 1 → Task 2 → Task 3 → Task 4 (sequential, each builds on previous)
