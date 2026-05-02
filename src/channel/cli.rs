use anyhow::Result;
use async_trait::async_trait;
use std::io::Write;

use super::{Channel, ChannelFactory};
use crate::message_bus::{BusRequest, BusResult};
use crate::session::{TaskHook, TaskState};
use tokio::sync::mpsc;

/// CLI channel implementation
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

/// CLI channel factory
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
    fn chat_id(&self) -> &str {
        &self.chat_id
    }
    fn name(&self) -> &str {
        "CLI"
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

    fn start(&self, inbound_tx: mpsc::Sender<BusRequest>) {
        let session_id = self.session_id();
        let channel_name = self.name().to_string();
        let prompt = self.prompt.clone();
        let hook = TaskHook::new(&session_id);

        // Use spawn_blocking for blocking stdin read to avoid blocking the tokio runtime
        tokio::task::spawn_blocking(move || {
            loop {
                print!("{}", prompt);
                let _ = std::io::stdout().flush();
                let mut input = String::new();
                match std::io::stdin().read_line(&mut input) {
                    Ok(0) => {
                        eprintln!("[{}] EOF, exiting read loop", channel_name);
                        return;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("[{}] Read failed: {}", channel_name, e);
                        continue;
                    }
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

                let rt = tokio::runtime::Handle::current();
                let tx = inbound_tx.clone();
                rt.block_on(async {
                    if let Err(e) = tx.send(request).await {
                        eprintln!("[{}] Failed to send inbound: {}", channel_name, e);
                    }
                });
            }
        });
    }
}
