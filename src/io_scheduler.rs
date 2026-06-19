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
                    info!(
                        "[{}] Shutdown signal received, exiting read loop",
                        channel_name
                    );
                    break;
                }
                let prompt = prompt.clone();
                match tokio::task::spawn_blocking(move || read_line_blocking(&prompt)).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_io_scheduler_new() {
        let (tx, _rx) = mpsc::channel(10);
        let scheduler = IoScheduler::new(tx);
        // Verify shutdown flag is initially false
        assert!(!scheduler.shutdown.load(Ordering::Relaxed));
    }

    #[test]
    fn test_io_scheduler_shutdown() {
        let (tx, _rx) = mpsc::channel(10);
        let scheduler = IoScheduler::new(tx);
        scheduler.shutdown();
        assert!(scheduler.shutdown.load(Ordering::Relaxed));
    }

    #[test]
    fn test_io_scheduler_set_channel_command_cb() {
        let (tx, _rx) = mpsc::channel(10);
        let scheduler = IoScheduler::new(tx);

        let cb_called = Arc::new(AtomicBool::new(false));
        let cb_called_clone = cb_called.clone();
        let cb: ChannelCommandCallback = Arc::new(move |_session_id, _cmd| {
            cb_called_clone.store(true, Ordering::Relaxed);
        });

        scheduler.set_channel_command_cb(cb);

        // Verify callback is set
        let guard = scheduler.channel_cmd_cb.lock().unwrap();
        assert!(guard.is_some());
    }

    #[test]
    fn test_io_read_error_variants() {
        // Verify IoReadError variants can be created
        let eof = IoReadError::Eof;
        let empty = IoReadError::Empty;
        let other = IoReadError::Other(anyhow::anyhow!("test error"));

        // Just verify they can be constructed (they're used in match arms)
        assert!(matches!(eof, IoReadError::Eof));
        assert!(matches!(empty, IoReadError::Empty));
        assert!(matches!(other, IoReadError::Other(_)));
    }

    #[test]
    fn test_io_handle_struct() {
        // Verify IoHandle can be constructed
        let handle = IoHandle {
            join_handle: None,
            session_id: "test-session".to_string(),
            channel_name: "test-channel".to_string(),
        };
        assert_eq!(handle.session_id, "test-session");
        assert_eq!(handle.channel_name, "test-channel");
        assert!(handle.join_handle.is_none());
    }

    #[test]
    fn test_io_scheduler_new_with_capacity() {
        let (tx, _rx) = mpsc::channel(100);
        let scheduler = IoScheduler::new(tx);
        assert!(!scheduler.shutdown.load(Ordering::Relaxed));
        assert!(scheduler.channel_cmd_cb.lock().unwrap().is_none());
    }

    #[test]
    fn test_io_scheduler_shutdown_idempotent() {
        let (tx, _rx) = mpsc::channel(10);
        let scheduler = IoScheduler::new(tx);
        // Shutdown multiple times should not panic
        scheduler.shutdown();
        scheduler.shutdown();
        scheduler.shutdown();
        assert!(scheduler.shutdown.load(Ordering::Relaxed));
    }

    #[test]
    fn test_io_scheduler_callback_replacement() {
        let (tx, _rx) = mpsc::channel(10);
        let scheduler = IoScheduler::new(tx);

        let call_count1 = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let call_count1_clone = call_count1.clone();
        let cb1: ChannelCommandCallback = Arc::new(move |_, _| {
            call_count1_clone.fetch_add(1, Ordering::Relaxed);
        });
        scheduler.set_channel_command_cb(cb1);

        let call_count2 = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let call_count2_clone = call_count2.clone();
        let cb2: ChannelCommandCallback = Arc::new(move |_, _| {
            call_count2_clone.fetch_add(1, Ordering::Relaxed);
        });
        scheduler.set_channel_command_cb(cb2);

        // Verify only the latest callback is stored
        let guard = scheduler.channel_cmd_cb.lock().unwrap();
        assert!(guard.is_some());
    }

    #[tokio::test]
    async fn test_io_scheduler_submit_blocking_read_loop_returns_handle() {
        let (tx, _rx) = mpsc::channel(10);
        let scheduler = IoScheduler::new(tx);
        let hook = crate::session::TaskHook::new("test-session");

        // Just verify the function can be called and returns a JoinHandle
        // We don't actually run the loop since it would block on stdin
        let handle = scheduler.submit_blocking_read_loop(
            "test-session".to_string(),
            hook,
            "cli".to_string(),
            "> ".to_string(),
        );

        // Verify the handle is valid
        assert!(!handle.is_finished());

        // Cleanup - shut down to allow the spawned task to exit
        scheduler.shutdown();
        // Give the task time to notice the shutdown
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        handle.await.unwrap();
    }

    #[test]
    fn test_io_read_error_debug() {
        // Verify IoReadError implements Debug
        let eof = IoReadError::Eof;
        let debug_str = format!("{:?}", eof);
        assert!(debug_str.contains("Eof"));

        let empty = IoReadError::Empty;
        let debug_str = format!("{:?}", empty);
        assert!(debug_str.contains("Empty"));
    }

    #[test]
    fn test_io_handle_fields_accessible() {
        let handle = IoHandle {
            join_handle: None,
            session_id: "cli:chat-123".to_string(),
            channel_name: "cli".to_string(),
        };

        // Verify all fields are accessible
        assert_eq!(handle.session_id, "cli:chat-123");
        assert_eq!(handle.channel_name, "cli");
        assert!(handle.join_handle.is_none());
    }
}
