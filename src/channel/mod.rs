mod cli;

#[allow(unused_imports)]
pub use cli::{CliChannel, CliChannelFactory};

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::io_scheduler::{IoHandle, IoScheduler};
use crate::message_bus::{BusRequest, BusResult, MessageBus};
use crate::session::TaskState;

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
    /// Start the channel's internal client read loop. Called by ChannelManager after creation.
    fn start(&self, inbound_tx: tokio::sync::mpsc::Sender<BusRequest>);
    /// Start the channel with an IoScheduler. Default delegates to start() for backward compatibility.
    fn start_with_scheduler(&self, _scheduler: &IoScheduler) -> IoHandle {
        // Default: no-op handle. Override in channel implementations that use the scheduler.
        IoHandle {
            join_handle: None,
            session_id: self.session_id(),
            channel_name: self.name().to_string(),
        }
    }
    /// Send output to client. Called by ChannelManager's outbound router.
    /// Default implementation delegates to write_output.
    async fn send_output(&mut self, result: &BusResult) -> Result<()> {
        self.write_output(result).await
    }
}

/// Channel factory trait, creates channels by type from config
pub trait ChannelFactory: Send + Sync {
    /// Return the type identifier for this factory
    fn channel_type(&self) -> &str;
    /// Create a channel instance from config
    fn create(&self, config: &serde_json::Value) -> Result<Box<dyn Channel>>;
}

/// ChannelManager: manages all channel instances and routes outbound messages.
/// Automatically registers all built-in channel factories on construction.
pub struct ChannelManager {
    channels: Arc<Mutex<HashMap<String, Box<dyn Channel>>>>,
    factories: HashMap<String, Box<dyn ChannelFactory>>,
    message_bus: Arc<MessageBus>,
    config: Arc<Config>,
    io_handles: Arc<Mutex<HashMap<String, IoHandle>>>,
}

impl ChannelManager {
    pub fn new(message_bus: Arc<MessageBus>, config: Arc<Config>) -> Self {
        let mut cm = Self {
            channels: Arc::new(Mutex::new(HashMap::new())),
            factories: HashMap::new(),
            message_bus,
            config,
            io_handles: Arc::new(Mutex::new(HashMap::new())),
        };
        // Auto-register all built-in channel factories
        cm.register_factory("cli", Box::new(CliChannelFactory));
        cm
    }

    /// Register a factory for a channel type
    pub fn register_factory(&mut self, type_name: &str, factory: Box<dyn ChannelFactory>) {
        self.factories.insert(type_name.to_string(), factory);
    }

    /// Initialize channels from stored config entries
    pub async fn init(&mut self) -> Result<()> {
        let channels = self.channels.clone();
        let io_handles = self.io_handles.clone();
        let scheduler = IoScheduler::new(self.message_bus.inbound_tx());

        for entry in &self.config.channels {
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

            // Start the channel's I/O loop via the scheduler
            let handle = channel.start_with_scheduler(&scheduler);

            // Insert into the shared map for outbound routing access
            channels.lock().await.insert(id.clone(), channel);
            io_handles.lock().await.insert(id, handle);
        }
        Ok(())
    }

    /// Run the outbound routing loop. Blocks the calling task until the
    /// outbound channel is closed. This is the main event loop for the system.
    pub async fn run(&self) {
        let channels = self.channels.clone();
        let outbound_rx = self.message_bus.outbound_rx();

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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_bus::BusResult;

    /// Dummy channel for testing
    struct TestChannel {
        channel_id: String,
        chat_id: String,
    }

    impl TestChannel {
        fn new(id: &str, chat: &str) -> Self {
            Self {
                channel_id: id.to_string(),
                chat_id: chat.to_string(),
            }
        }
    }

    #[async_trait]
    impl Channel for TestChannel {
        fn id(&self) -> &str {
            &self.channel_id
        }
        fn name(&self) -> &str {
            "Test"
        }
        fn chat_id(&self) -> &str {
            &self.chat_id
        }
        async fn read_input(&mut self) -> Result<String> {
            Ok("test input".to_string())
        }
        async fn write_output(&mut self, _result: &BusResult) -> Result<()> {
            Ok(())
        }
        async fn write_status(&mut self, _session_id: &str, _state: &TaskState) -> Result<()> {
            Ok(())
        }
        async fn prepare_inject(&self) -> Result<String> {
            Ok(String::new())
        }
        fn start(&self, _inbound_tx: tokio::sync::mpsc::Sender<BusRequest>) {
            // no-op for test
        }
    }

    /// Dummy factory for TestChannel
    struct TestChannelFactory;

    impl ChannelFactory for TestChannelFactory {
        fn channel_type(&self) -> &str {
            "test"
        }
        fn create(&self, _config: &serde_json::Value) -> Result<Box<dyn Channel>> {
            Ok(Box::new(TestChannel::new("test", "default")))
        }
    }

    #[test]
    fn test_channel_session_id() {
        let ch = TestChannel::new("cli", "abc123");
        assert_eq!(ch.session_id(), "cli:abc123");
    }

    #[test]
    fn test_channel_id_name_chat() {
        let ch = TestChannel::new("web", "chat1");
        assert_eq!(ch.id(), "web");
        assert_eq!(ch.name(), "Test");
        assert_eq!(ch.chat_id(), "chat1");
    }

    #[test]
    fn test_factories_hashmap() {
        let factories: HashMap<String, Box<dyn ChannelFactory>> = HashMap::new();
        assert!(factories.is_empty());
    }
}
