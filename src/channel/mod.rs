mod cli;

#[allow(unused_imports)]
pub use cli::{CliChannel, CliChannelFactory};

use anyhow::Result;
use async_trait::async_trait;

use crate::config::ChannelEntry;
use crate::message_bus::{BusRequest, BusResult, MessageBus};
use crate::session::{TaskHook, TaskState};

/// Channel trait, abstracts all I/O channels
#[async_trait]
pub trait Channel: Send + Sync {
    /// Channel unique identifier
    fn id(&self) -> &str;
    /// Channel name (for logging and debugging)
    fn name(&self) -> &str;
    /// Current session chat_id (unique per conversation session)
    fn chat_id(&self) -> &str;
    /// Read one line of user input
    async fn read_input(&mut self) -> Result<String>;
    /// Output final result
    async fn write_output(&mut self, result: &BusResult) -> Result<()>;
    /// Output intermediate status info (e.g. tool execution progress)
    async fn write_status(&mut self, session_id: &str, state: &TaskState) -> Result<()>;
    /// Prepare extra info to inject into Context (optional)
    async fn prepare_inject(&self) -> Result<String>;
    /// Generate session_id (channel_id:chat_id)
    fn session_id(&self) -> String {
        format!("{}:{}", self.id(), self.chat_id())
    }
}

/// Channel factory trait, creates channels by type from config
pub trait ChannelFactory: Send + Sync {
    /// Return the type identifier for this factory
    fn channel_type(&self) -> &str;
    /// Create a channel instance from config
    fn create(&self, config: &serde_json::Value) -> Result<Box<dyn Channel>>;
}

/// ChannelManager: manages all channel instances and drives I/O
pub struct ChannelManager {
    channels: Vec<Box<dyn Channel>>,
    message_bus: std::sync::Arc<MessageBus>,
}

impl ChannelManager {
    pub fn new(message_bus: std::sync::Arc<MessageBus>) -> Self {
        Self {
            channels: Vec::new(),
            message_bus,
        }
    }

    /// Create and register channels from config and factory list
    pub fn init_from_config(
        &mut self,
        entries: &[ChannelEntry],
        factories: &[Box<dyn ChannelFactory>],
    ) -> Result<()> {
        for entry in entries {
            if !entry.enabled {
                continue;
            }
            let factory = factories.iter()
                .find(|f| f.channel_type() == entry.r#type)
                .ok_or_else(|| anyhow::anyhow!("Unregistered channel type: {}", entry.r#type))?;
            let channel = factory.create(&entry.config)?;
            eprintln!("Registered channel: {} ({})", channel.name(), channel.session_id());
            self.channels.push(channel);
        }
        Ok(())
    }

    /// Register a channel
    pub fn register(&mut self, channel: Box<dyn Channel>) {
        eprintln!("Registered channel: {}", channel.name());
        self.channels.push(channel);
    }

    /// Start I/O loops for all channels
    pub async fn run(&mut self) -> Result<()> {
        // Take ownership of channels to move them into spawned tasks
        let channels = std::mem::take(&mut self.channels);

        for mut channel in channels {
            let session_id = channel.session_id();

            // Create status event channel for each channel
            let (status_tx, mut status_rx) = tokio::sync::mpsc::channel::<(String, TaskState)>(32);

            // Send state changes to Channel via TaskHook
            let hook = TaskHook::new(&session_id).with_status_channel(status_tx);

            // Background task: listen for status events from TaskHook, display via Channel
            let sid = session_id.clone();
            let channel_name = channel.name().to_string();
            tokio::spawn(async move {
                while let Some((_session_id, state)) = status_rx.recv().await {
                    match &state {
                        TaskState::Running { current_iteration } => {
                            eprintln!("[{}] [{}] Running - iteration {}", channel_name, sid, current_iteration);
                        }
                        TaskState::Completed { .. } => {
                            eprintln!("[{}] [{}] Completed", channel_name, sid);
                        }
                        TaskState::Failed { error } => {
                            eprintln!("[{}] [{}] Failed - {}", channel_name, sid, error);
                        }
                        TaskState::Pending => {}
                    }
                }
            });

            // Spawn a concurrent task for each channel's I/O loop
            let message_bus = self.message_bus.clone();
            tokio::spawn(async move {
                loop {
                    let input = match channel.read_input().await {
                        Ok(s) if s.is_empty() => continue,
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("[{}] Read failed: {}", channel.name(), e);
                            continue;
                        }
                    };

                    // ChannelManager prepares inject content ahead of time
                    let channel_inject = match channel.prepare_inject().await {
                        Ok(s) => Some(s),
                        Err(e) => {
                            eprintln!("[{}] Prepare inject failed: {}", channel.name(), e);
                            None
                        }
                    };

                    let request = BusRequest {
                        session_id: channel.session_id(),
                        content: input,
                        channel_inject,
                        hook: hook.clone(),
                    };

                    let bus = message_bus.clone();
                    let result = match tokio::spawn(async move {
                        bus.send(request).await
                    }).await {
                        Ok(Ok(r)) => r,
                        Ok(Err(e)) => {
                            eprintln!("[{}] Bus error: {}", channel.name(), e);
                            continue;
                        }
                        Err(e) => {
                            eprintln!("[{}] Spawn join error: {}", channel.name(), e);
                            continue;
                        }
                    };

                    if let Err(e) = channel.write_output(&result).await {
                        eprintln!("[{}] Write output failed: {}", channel.name(), e);
                    }
                }
            });
        }

        // Wait forever — each channel runs in its own spawned task
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    }
}
