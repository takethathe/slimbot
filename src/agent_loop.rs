use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{broadcast, mpsc, Notify};

use crate::commands::{classify_command, CommandTier};
use crate::config::Config;
use crate::config_scheme::ConfigScheme;
use crate::consolidate::Consolidator;
use crate::memory::{MemoryStore, SharedMemoryStore};
use crate::message_bus::{BusRequest, BusResult, MessageBus};
use crate::path::PathManager;
use crate::provider::{OpenAIProvider, Provider};
use crate::runner::{AgentResult, AgentRunner};
use crate::session::{SessionManager, SessionTaskBuilder, SharedSessionManager, TaskHook, ensure_session};
use crate::tool::{Tool, ToolManager};
use crate::{error, info, warn_log};

/// Orchestrates graceful shutdown of all components.
async fn graceful_shutdown(
    channel_manager: &crate::channel::ChannelManager,
    session_manager: &SharedSessionManager,
    consolidator: &Consolidator,
    memory_store: &SharedMemoryStore,
) {
    // Phase 1: Stop all channel I/O threads — no more user input.
    channel_manager.shutdown().await;
    shutdown_session_memory(session_manager, consolidator, memory_store).await;
    info!("[AgentLoop] Graceful shutdown complete");
}

/// Common shutdown sequence: consolidator, sessions, memory store.
/// Used by both channel-based and CLI-only modes.
async fn shutdown_session_memory(
    session_manager: &SharedSessionManager,
    consolidator: &Consolidator,
    memory_store: &SharedMemoryStore,
) {
    consolidator.shutdown();
    {
        let guard = session_manager.lock().await;
        guard.shutdown_all();
        drop(guard);
    }
    session_manager.lock().await.wait_all_idle().await;
    session_manager.lock().await.sync_all_meta();
    {
        let guard = memory_store.lock().await;
        let _ = guard.sync_all();
        drop(guard);
    }
}

pub struct AgentLoop {
    config: Arc<Config>,
    workspace_dir: PathBuf,
    provider: Arc<dyn Provider>,
    tool_manager: Arc<ToolManager>,
    session_manager: SharedSessionManager,
    memory_store: SharedMemoryStore,
    message_bus: Arc<MessageBus>,
    consolidator: Arc<Consolidator>,
    /// Broadcast sender: triggers shutdown of inbound loop and I/O schedulers.
    shutdown_tx: broadcast::Sender<()>,
}

/// Returned by `AgentLoop::run()` so the caller can publish input and
/// trigger shutdown from the main (CLI I/O) thread.
pub struct ShutdownHandle {
    shutdown_tx: broadcast::Sender<()>,
    inbound_tx: mpsc::Sender<BusRequest>,
}

impl ShutdownHandle {
    /// Blocking stdin read loop for the main thread.
    /// Reads user input, sends to the inbound listener, and detects
    /// channel-tier commands (e.g. /quit) to trigger shutdown.
    ///
    /// Runs entirely on the calling thread — no tokio::spawn.
    pub fn run_main_thread_loop(&self, session_id: &str, prompt: &str) {
        loop {
            // Check shutdown flag before blocking on stdin.
            // The shutdown broadcast sender is checked via try_recv to see
            // if a /quit was already processed.
            if self.shutdown_tx.receiver_count() == 0 {
                break;
            }

            // Block on stdin.
            let mut input = String::new();
            print!("{}", prompt);
            let _ = std::io::stdout().flush();
            match std::io::stdin().read_line(&mut input) {
                Ok(0) => {
                    info!("[main] EOF, exiting stdin loop");
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    warn_log!("[main] Read failed: {}", e);
                    continue;
                }
            }

            let input = input.trim().to_string();
            if input.is_empty() {
                continue;
            }

            // Channel-tier command: trigger shutdown.
            let cmd = classify_command(&input);
            if cmd.is_command && cmd.tier == CommandTier::Channel {
                info!("[main] Channel command detected: {}", input);
                let _ = self.shutdown_tx.send(());
                break;
            }

            // Normal input: send to inbound.
            // Use try_send because the main thread is also the tokio runtime thread;
            // blocking_send would panic.
            let hook = TaskHook::new(session_id);
            let request = BusRequest {
                session_id: session_id.to_string(),
                content: input,
                channel_inject: None,
                hook,
            };
            if let Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) = self.inbound_tx.try_send(request) {
                error!("[main] Inbound channel closed");
                break;
            }
        }
    }
}

impl AgentLoop {
    pub async fn from_config(
        paths: &PathManager,
        message_bus: Arc<MessageBus>,
        config: Arc<Config>,
    ) -> Result<Self> {

        let provider_config = config
            .providers
            .get(&config.agent.provider)
            .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found", config.agent.provider))?;
        let provider = Arc::new(OpenAIProvider::new(provider_config));

        let mut tool_manager = ToolManager::new(paths.workspace_dir().to_path_buf());
        tool_manager.init_from_config(&config.tools);

        Self::from_parts(paths, message_bus, config, provider, tool_manager).await
    }

    /// Create an AgentLoop from a pre-configured ToolManager.
    /// Used by gateway mode to register extra tools (message, cron) before startup.
    pub async fn from_config_with_tools(
        paths: &PathManager,
        message_bus: Arc<MessageBus>,
        config: Arc<Config>,
        tool_manager: ToolManager,
    ) -> Result<Self> {
        let provider_config = config
            .providers
            .get(&config.agent.provider)
            .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found", config.agent.provider))?;
        let provider = Arc::new(OpenAIProvider::new(provider_config));

        Self::from_parts(paths, message_bus, config, provider, tool_manager).await
    }

    async fn from_parts(
        paths: &PathManager,
        message_bus: Arc<MessageBus>,
        config: Arc<Config>,
        provider: Arc<dyn Provider>,
        tool_manager: ToolManager,
    ) -> Result<Self> {
        let session_manager = SessionManager::new(paths.session_dir())?;
        let session_manager_arc = Arc::new(tokio::sync::Mutex::new(session_manager));

        let memory_store = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(paths.workspace_dir())));
        memory_store.lock().await.init()?;

        let consolidator = Arc::new(Consolidator::new(
            provider.clone(),
            session_manager_arc.clone(),
            memory_store.clone(),
            config.agent.context_window_tokens,
            ConfigScheme::DEFAULT_MAX_TOKENS,
        ));

        let (shutdown_tx, _) = broadcast::channel::<()>(1);

        Ok(Self {
            config,
            workspace_dir: paths.workspace_dir().to_path_buf(),
            provider,
            tool_manager: Arc::new(tool_manager),
            session_manager: session_manager_arc,
            memory_store,
            message_bus,
            consolidator,
            shutdown_tx,
        })
    }

    pub fn register_tool(&mut self, _tool: Box<dyn Tool>) {
        // Reserved: in production, should reload config or maintain registration list
    }

    pub fn session_manager(&self) -> SharedSessionManager {
        self.session_manager.clone()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Run a single task within the given session.
    pub async fn run_task(
        &self,
        session_id: &str,
        content: String,
        hook: TaskHook,
        channel_inject: Option<String>,
    ) -> AgentResult {
        let cancel_token = self.session_manager.lock().await
            .session_cancel_token(session_id);
        let runner = AgentRunner::new(
            self.tool_manager.clone(),
            self.provider.clone(),
            self.session_manager.clone(),
            self.config.agent.clone(),
            self.workspace_dir.clone(),
            self.memory_store.clone(),
            channel_inject,
            Some(self.consolidator.clone()),
            cancel_token,
        );
        runner.run(content, hook, session_id).await
    }

    /// Start the inbound listener task.
    pub fn start_inbound(&self, done: Option<Arc<Notify>>) {
        let inbound_rx = self.message_bus.inbound_rx();
        let session_manager = self.session_manager.clone();
        let tool_manager = self.tool_manager.clone();
        let provider = self.provider.clone();
        let config = self.config.agent.clone();
        let workspace_dir = self.workspace_dir.clone();
        let memory_store = self.memory_store.clone();
        let outbound_tx = self.message_bus.outbound_tx();
        let consolidator = self.consolidator.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx.recv() => {
                        info!("[AgentLoop] Shutdown signal received, stopping inbound listener");
                        break;
                    }
                    request = async {
                        let mut rx_guard = inbound_rx.lock().await;
                        rx_guard.recv().await
                    } => {
                        let request = match request {
                            Some(r) => r,
                            None => break, // channel closed
                        };

                        // Check for AgentLoop-tier commands (/stop, /clear, /status)
                        // Channel-tier commands (/quit) are intercepted before reaching here
                        let cmd = classify_command(&request.content);
                        if cmd.is_command && cmd.tier == CommandTier::AgentLoop {
                            let sid = request.session_id.clone();
                            let (msg, _shutdown) = Self::handle_agent_loop_command(
                                &session_manager,
                                &outbound_tx,
                                &sid,
                                &request.content,
                            ).await;
                            if !msg.is_empty() {
                                let _ = outbound_tx.send(BusResult {
                                    session_id: sid.clone(),
                                    task_id: String::new(),
                                    content: msg,
                                }).await;
                            }
                            continue;
                        }

                        if let Err(e) = ensure_session(&session_manager, &request.session_id).await {
                            error!("[AgentLoop] Session error: {}", e);
                            continue;
                        }

                        // Get the session's cancellation token for task-level cancellation.
                        let cancel_token = {
                            let guard = session_manager.lock().await;
                            guard.session_cancel_token(&request.session_id)
                        };

                        let task = SessionTaskBuilder::new(
                            request.session_id.clone(),
                            request.content,
                            request.hook,
                        )
                        .session_manager(session_manager.clone())
                        .tool_manager(tool_manager.clone())
                        .provider(provider.clone())
                        .config(config.clone())
                        .workspace_dir(workspace_dir.clone())
                        .memory_store(memory_store.clone())
                        .outbound_tx(outbound_tx.clone())
                        .channel_inject(request.channel_inject)
                        .consolidator(Some(consolidator.clone()))
                        .cancel_token(cancel_token)
                        .build();

                        let mut guard = session_manager.lock().await;
                        let _ = guard.submit_task(task).await;
                    }
                }
            }

            if let Some(done) = done {
                done.notify_one();
            }
        });
    }

    /// Handle an AgentLoop-tier command.
    /// For /stop: cancels the session and enqueues a sentinel task; returns empty response
    /// (the sentinel task's result signals completion to the user).
    /// For /clear and /status: returns a response message directly.
    async fn handle_agent_loop_command(
        session_manager: &SharedSessionManager,
        outbound_tx: &tokio::sync::mpsc::Sender<BusResult>,
        session_id: &str,
        command: &str,
    ) -> (String, bool) {
        match command {
            "/stop" => {
                // Cancel all queued tasks and get a fresh token.
                // Already-queued tasks hold clones of the old token → they see cancellation.
                // The sentinel task uses the new token → runs normally, detects "/stop" content.
                let new_token = {
                    let guard = session_manager.lock().await;
                    guard.cancel_and_reset_session(session_id)
                };

                let task = match new_token {
                    Some(ct) => SessionTaskBuilder::new(
                        session_id.to_string(),
                        "/stop".to_string(),
                        TaskHook::new(session_id),
                    )
                    .session_manager(session_manager.clone())
                    .outbound_tx(outbound_tx.clone())
                    .cancel_token(Some(ct))
                    .build(),
                    None => {
                        return ("No session to stop.".to_string(), false);
                    }
                };

                let mut guard = session_manager.lock().await;
                let _ = guard.submit_task(task).await;
                drop(guard);

                // Empty response — the sentinel task sends the real response.
                (String::new(), false)
            }
            "/clear" | "/new" => {
                let mut guard = session_manager.lock().await;
                guard.clear_session(session_id);
                drop(guard);
                ("Session cleared. Starting fresh.".to_string(), false)
            }
            "/status" => {
                let guard = session_manager.lock().await;
                let msg_count = guard.message_count(session_id);
                drop(guard);

                let status = format!(
                    "Session: {}\nMessages: {}",
                    session_id, msg_count
                );
                (status, false)
            }
            _ => (String::new(), false),
        }
    }

    /// Spawn the inbound and outbound background tasks and return a
    /// `ShutdownHandle`. The caller (typically the main thread in CLI
    /// multi-turn mode) uses the handle to publish user input and trigger
    /// coordinated shutdown.
    pub fn run(&self, channel_manager: &Arc<crate::channel::ChannelManager>) -> ShutdownHandle {
        // Wire up the channel-layer command callback (for /quit).
        // The callback sends a shutdown broadcast directly, bypassing Notify.
        channel_manager.set_quit_broadcast(self.shutdown_tx.clone());

        // Start inbound listener
        self.start_inbound(None);

        // Spawn outbound router with shutdown signal
        let outbound_task_cm = channel_manager.clone();
        let shutdown_rx = self.shutdown_tx.subscribe();
        tokio::spawn(async move {
            outbound_task_cm.run_with_shutdown(shutdown_rx).await;
        });

        ShutdownHandle {
            shutdown_tx: self.shutdown_tx.clone(),
            inbound_tx: self.message_bus.inbound_tx(),
        }
    }

    /// Graceful shutdown of all components. Called by the main thread after
    /// the stdin loop exits (triggered by /quit).
    pub async fn graceful_shutdown(
        &self,
        channel_manager: &Arc<crate::channel::ChannelManager>,
    ) {
        graceful_shutdown(
            channel_manager,
            &self.session_manager,
            &self.consolidator,
            &self.memory_store,
        ).await;
    }

    /// Shutdown for CLI mode (no ChannelManager). Shuts down consolidator,
    /// session manager, and memory store.
    pub async fn shutdown_for_cli(&self) {
        shutdown_session_memory(&self.session_manager, &self.consolidator, &self.memory_store).await;
        info!("[AgentLoop] CLI session shutdown complete");
    }
}
