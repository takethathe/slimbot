use anyhow::Result;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::commands::CommandTier;
use crate::message_bus::BusRequest;
use crate::session::TaskHook;
use crate::{error, info, warn_log};

/// Callback invoked when a channel-tier command (e.g. /quit) is detected.
/// The callback is responsible for triggering shutdown.
pub type ChannelCommandCallback = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Handle returned when a channel starts its I/O loop.
/// Gives ChannelManager lifecycle visibility over channel I/O.
pub struct IoHandle {
    pub join_handle: Option<JoinHandle<()>>,
    pub session_id: String,
    pub channel_name: String,
}

/// Coordinates I/O execution for channels, routing blocking vs async I/O
/// to the appropriate executor.
pub struct IoScheduler {
    inbound_tx: mpsc::Sender<BusRequest>,
    shutdown: Arc<AtomicBool>,
    /// Optional callback invoked when a channel-tier command (e.g. /quit) is detected.
    /// Called with (session_id, command_text) before the read loop exits.
    channel_cmd_cb: Arc<Mutex<Option<ChannelCommandCallback>>>,
}

impl IoScheduler {
    pub fn new(inbound_tx: mpsc::Sender<BusRequest>) -> Self {
        Self {
            inbound_tx,
            shutdown: Arc::new(AtomicBool::new(false)),
            channel_cmd_cb: Arc::new(Mutex::new(None)),
        }
    }

    /// Set the callback invoked when channel-tier commands are detected in user input.
    pub fn set_channel_command_cb(&self, cb: ChannelCommandCallback) {
        let mut guard = self.channel_cmd_cb.lock().unwrap();
        *guard = Some(cb);
    }

    /// Signal all I/O loops to exit on their next iteration.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Start a blocking read loop for a CLI-style channel.
    /// Spawns an async task that repeatedly calls `spawn_blocking` to read
    /// one line from stdin and sends results to MessageBus.
    /// Returns a JoinHandle for the outer async loop.
    pub fn submit_blocking_read_loop(
        &self,
        session_id: String,
        hook: TaskHook,
        channel_name: String,
        prompt: String,
    ) -> JoinHandle<()> {
        let tx = self.inbound_tx.clone();
        let shutdown = self.shutdown.clone();
        let channel_cmd_cb = self.channel_cmd_cb.clone();
        tokio::spawn(async move {
            loop {
                if shutdown.load(Ordering::Relaxed) {
                    info!("[{}] Shutdown signal received, exiting read loop", channel_name);
                    break;
                }
                let prompt = prompt.clone();
                match tokio::task::spawn_blocking(move || {
                    read_line_blocking(&prompt)
                })
                .await
                {
                    Ok(Ok(input)) => {
                        let cmd = crate::commands::classify_command(&input);
                        if cmd.is_command && cmd.tier == CommandTier::Channel {
                            info!("[{}] Channel command intercepted: {}", channel_name, input);
                            let cb = {
                                let guard = channel_cmd_cb.lock().unwrap();
                                guard.clone()
                            };
                            if let Some(cb) = cb {
                                cb(&session_id, &input);
                            }
                            break;
                        }

                        let request = BusRequest {
                            session_id: session_id.clone(),
                            content: input,
                            channel_inject: None,
                            hook: hook.clone(),
                        };
                        if let Err(e) = tx.send(request).await {
                            error!("[{}] Failed to send inbound: {}", channel_name, e);
                            break;
                        }
                    }
                    Ok(Err(IoReadError::Eof)) => {
                        info!("[{}] EOF, exiting read loop", channel_name);
                        break;
                    }
                    Ok(Err(IoReadError::Empty)) => {
                        continue;
                    }
                    Ok(Err(IoReadError::Other(e))) => {
                        warn_log!("[{}] Read failed: {}", channel_name, e);
                        continue;
                    }
                    Err(e) => {
                        error!("[{}] Read task panicked: {}", channel_name, e);
                        break;
                    }
                }
            }
        })
    }
}

/// Blocking read error kinds.
#[derive(Debug)]
pub enum IoReadError {
    /// End of input stream (e.g., stdin closed)
    Eof,
    /// Empty input, should be skipped
    Empty,
    /// Other I/O error
    Other(anyhow::Error),
}

/// Helper: blocking read of one line from stdin.
/// Returns IoReadError::Eof on 0 bytes, IoReadError::Empty on whitespace-only.
fn read_line_blocking(prompt: &str) -> Result<String, IoReadError> {
    print!("{}", prompt);
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    let n = std::io::stdin()
        .read_line(&mut input)
        .map_err(|e| IoReadError::Other(anyhow::anyhow!(e)))?;
    if n == 0 {
        return Err(IoReadError::Eof);
    }
    let input = input.trim().to_string();
    if input.is_empty() {
        return Err(IoReadError::Empty);
    }
    Ok(input)
}
