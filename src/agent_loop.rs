use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::{Notify, broadcast, mpsc};

use crate::commands::{CommandTier, classify_command};
use crate::config::Config;
use crate::consolidate::Consolidator;
use crate::memory::{MemoryStore, SharedMemoryStore};
use crate::message_bus::{BusRequest, BusResult, MessageBus};
use crate::path::PathManager;
use crate::provider::{OpenAIProvider, Provider};
use crate::runner::{AgentResult, AgentRunner};
use crate::session::{
    SessionManager, SessionTaskBuilder, SharedSessionManager, TaskHook, ensure_session,
};
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
    session_manager.lock().await.graceful_shutdown().await;
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
    /// Owned inbound receiver, consumed once by `start_inbound`.
    inbound_rx: Mutex<Option<mpsc::Receiver<BusRequest>>>,
    consolidator: Arc<Consolidator>,
    /// Broadcast sender: triggers shutdown of inbound loop and I/O schedulers.
    shutdown_tx: broadcast::Sender<()>,
    /// Pre-built runner with stable components, reused across tasks.
    runner: AgentRunner,
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
            if let Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) =
                self.inbound_tx.try_send(request)
            {
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
        inbound_rx: mpsc::Receiver<BusRequest>,
        config: Arc<Config>,
    ) -> Result<Self> {
        let provider_config = config
            .providers
            .get(&config.agent.provider)
            .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found", config.agent.provider))?;
        let provider = Arc::new(OpenAIProvider::new(provider_config));

        let mut tool_manager = ToolManager::new(paths.workspace_dir().to_path_buf());
        tool_manager.init_from_config(&config.tools);

        Self::from_parts(
            paths,
            message_bus,
            inbound_rx,
            config,
            provider,
            tool_manager,
        )
        .await
    }

    /// Create an AgentLoop from a pre-configured ToolManager.
    /// Used by gateway mode to register extra tools (message, cron) before startup.
    pub async fn from_config_with_tools(
        paths: &PathManager,
        message_bus: Arc<MessageBus>,
        inbound_rx: mpsc::Receiver<BusRequest>,
        config: Arc<Config>,
        tool_manager: ToolManager,
    ) -> Result<Self> {
        let provider_config = config
            .providers
            .get(&config.agent.provider)
            .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found", config.agent.provider))?;
        let provider = Arc::new(OpenAIProvider::new(provider_config));

        Self::from_parts(
            paths,
            message_bus,
            inbound_rx,
            config,
            provider,
            tool_manager,
        )
        .await
    }

    async fn from_parts(
        paths: &PathManager,
        message_bus: Arc<MessageBus>,
        inbound_rx: mpsc::Receiver<BusRequest>,
        config: Arc<Config>,
        provider: Arc<dyn Provider>,
        tool_manager: ToolManager,
    ) -> Result<Self> {
        let session_manager = SessionManager::new(paths.session_dir())?;
        let session_manager_arc = Arc::new(tokio::sync::Mutex::new(session_manager));

        let memory_store = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(
            paths.workspace_dir(),
        )));
        memory_store.lock().await.init()?;

        let consolidator = Arc::new(Consolidator::new(
            provider.clone(),
            session_manager_arc.clone(),
            memory_store.clone(),
            config.agent.context_window_tokens,
        ));

        let (shutdown_tx, _) = broadcast::channel::<()>(1);

        let tool_manager_arc = Arc::new(tool_manager);

        let runner = AgentRunner::new(
            tool_manager_arc.clone(),
            provider.clone(),
            session_manager_arc.clone(),
            config.agent.clone(),
            paths.workspace_dir().to_path_buf(),
            memory_store.clone(),
            Some(consolidator.clone()),
        );

        Ok(Self {
            config,
            workspace_dir: paths.workspace_dir().to_path_buf(),
            provider,
            tool_manager: tool_manager_arc,
            session_manager: session_manager_arc,
            memory_store,
            message_bus,
            inbound_rx: Mutex::new(Some(inbound_rx)),
            consolidator,
            shutdown_tx,
            runner,
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
        origin_channel: Option<String>,
        origin_chat_id: Option<String>,
    ) -> AgentResult {
        // Ensure session exists (cron/heartbeat tasks may use new session IDs)
        if let Err(e) = ensure_session(&self.session_manager, session_id).await {
            error!("[AgentLoop] Failed to ensure session {}: {}", session_id, e);
            return AgentResult {
                success: false,
                content: format!("Session error: {}", e),
                ..Default::default()
            };
        }

        let cancel_token = {
            let guard = self.session_manager.lock().await;
            guard.session_cancel_token(session_id)
        };
        let result = self
            .runner
            .run(
                content,
                hook,
                session_id,
                channel_inject,
                cancel_token,
                origin_channel,
                origin_chat_id,
            )
            .await;

        // Suppress final response if the message tool already delivered output this turn.
        // This prevents duplicate messages when a cron/heartbeat task uses the message tool.
        if result.message_sent {
            return AgentResult {
                content: String::new(),
                ..result
            };
        }

        result
    }

    /// Start the inbound listener task.
    pub fn start_inbound(&self, done: Option<Arc<Notify>>) {
        let mut inbound_rx = self
            .inbound_rx
            .lock()
            .unwrap()
            .take()
            .expect("inbound_rx already consumed by a previous start_inbound call");
        let session_manager = self.session_manager.clone();
        let tool_manager = self.tool_manager.clone();
        let provider = self.provider.clone();
        let config = self.config.agent.clone();
        let workspace_dir = self.workspace_dir.clone();
        let memory_store = self.memory_store.clone();
        let outbound_tx = self.message_bus.outbound_tx();
        let consolidator = self.consolidator.clone();
        let message_bus = self.message_bus.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx.recv() => {
                        info!("[AgentLoop] Shutdown signal received, stopping inbound listener");
                        break;
                    }
                    request = inbound_rx.recv() => {
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
                                message_bus.publish_outbound(BusResult {
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
                let msg_count = guard.total_message_count(session_id);
                drop(guard);

                let status = format!("Session: {}\nMessages: {}", session_id, msg_count);
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
    pub async fn graceful_shutdown(&self, channel_manager: &Arc<crate::channel::ChannelManager>) {
        graceful_shutdown(
            channel_manager,
            &self.session_manager,
            &self.consolidator,
            &self.memory_store,
        )
        .await;
    }

    /// Shutdown for CLI mode (no ChannelManager). Shuts down consolidator,
    /// session manager, and memory store.
    pub async fn shutdown_for_cli(&self) {
        shutdown_session_memory(
            &self.session_manager,
            &self.consolidator,
            &self.memory_store,
        )
        .await;
        info!("[AgentLoop] CLI session shutdown complete");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Message;

    fn make_test_paths(tmp: &tempfile::TempDir) -> PathManager {
        let data_dir = tmp.path().join("data");
        let workspace_dir = data_dir.join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        // Create a minimal config file
        let config_path = data_dir.join("config.json");
        let config = crate::config::Config {
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                max_tool_result_chars: 8000,
                persist_tool_results: false,
                context_window_tokens: 32768,
                unknown: Default::default(),
            },
            providers: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "default".to_string(),
                    crate::config::ProviderConfig {
                        r#type: "openai".to_string(),
                        api_url: "".to_string(),
                        base_url: "https://api.openai.com".to_string(),
                        api_key: "test-key".to_string(),
                        model: "gpt-4o".to_string(),
                        temperature: 0.7,
                        max_tokens: 4096,
                        prompt_cache_enabled: false,
                        unknown: Default::default(),
                    },
                );
                map
            },
            tools: vec![],
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                cron: crate::config::CronConfig { enabled: false },
                heartbeat: crate::config::HeartbeatConfig {
                    enabled: false,
                    interval_s: 60,
                },
            },
        };
        let config_json = serde_json::to_string_pretty(&config).unwrap();
        std::fs::write(&config_path, config_json).unwrap();

        PathManager::resolve(
            Some(config_path.to_str().unwrap()),
            Some(data_dir.to_str().unwrap()),
            Some(workspace_dir.to_str().unwrap()),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn test_agent_loop_from_config() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(&tmp);
        let (message_bus, _receivers) = MessageBus::new();
        let config = Arc::new(crate::config::Config {
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                max_tool_result_chars: 8000,
                persist_tool_results: false,
                context_window_tokens: 32768,
                unknown: Default::default(),
            },
            providers: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "default".to_string(),
                    crate::config::ProviderConfig {
                        r#type: "openai".to_string(),
                        api_url: "".to_string(),
                        base_url: "https://api.openai.com".to_string(),
                        api_key: "test-key".to_string(),
                        model: "gpt-4o".to_string(),
                        temperature: 0.7,
                        max_tokens: 4096,
                        prompt_cache_enabled: false,
                        unknown: Default::default(),
                    },
                );
                map
            },
            tools: vec![],
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                cron: crate::config::CronConfig { enabled: false },
                heartbeat: crate::config::HeartbeatConfig {
                    enabled: false,
                    interval_s: 60,
                },
            },
        });

        let result = AgentLoop::from_config(&paths, message_bus, _receivers.inbound, config).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_agent_loop_from_config_with_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(&tmp);
        let (message_bus, _receivers) = MessageBus::new();
        let config = Arc::new(crate::config::Config {
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                max_tool_result_chars: 8000,
                persist_tool_results: false,
                context_window_tokens: 32768,
                unknown: Default::default(),
            },
            providers: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "default".to_string(),
                    crate::config::ProviderConfig {
                        r#type: "openai".to_string(),
                        api_url: "".to_string(),
                        base_url: "https://api.openai.com".to_string(),
                        api_key: "test-key".to_string(),
                        model: "gpt-4o".to_string(),
                        temperature: 0.7,
                        max_tokens: 4096,
                        prompt_cache_enabled: false,
                        unknown: Default::default(),
                    },
                );
                map
            },
            tools: vec![],
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                cron: crate::config::CronConfig { enabled: false },
                heartbeat: crate::config::HeartbeatConfig {
                    enabled: false,
                    interval_s: 60,
                },
            },
        });

        let tool_manager = ToolManager::new(paths.workspace_dir().to_path_buf());
        let result = AgentLoop::from_config_with_tools(
            &paths,
            message_bus,
            _receivers.inbound,
            config,
            tool_manager,
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_agent_loop_session_manager() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(&tmp);
        let (message_bus, _receivers) = MessageBus::new();
        let config = Arc::new(crate::config::Config {
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                max_tool_result_chars: 8000,
                persist_tool_results: false,
                context_window_tokens: 32768,
                unknown: Default::default(),
            },
            providers: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "default".to_string(),
                    crate::config::ProviderConfig {
                        r#type: "openai".to_string(),
                        api_url: "".to_string(),
                        base_url: "https://api.openai.com".to_string(),
                        api_key: "test-key".to_string(),
                        model: "gpt-4o".to_string(),
                        temperature: 0.7,
                        max_tokens: 4096,
                        prompt_cache_enabled: false,
                        unknown: Default::default(),
                    },
                );
                map
            },
            tools: vec![],
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                cron: crate::config::CronConfig { enabled: false },
                heartbeat: crate::config::HeartbeatConfig {
                    enabled: false,
                    interval_s: 60,
                },
            },
        });

        let agent_loop = AgentLoop::from_config(&paths, message_bus, _receivers.inbound, config)
            .await
            .unwrap();
        let sm = agent_loop.session_manager();
        assert!(sm.lock().await.list_persisted_sessions("").is_empty());
    }

    #[tokio::test]
    async fn test_agent_loop_config() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(&tmp);
        let (message_bus, _receivers) = MessageBus::new();
        let config = Arc::new(crate::config::Config {
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                max_tool_result_chars: 8000,
                persist_tool_results: false,
                context_window_tokens: 32768,
                unknown: Default::default(),
            },
            providers: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "default".to_string(),
                    crate::config::ProviderConfig {
                        r#type: "openai".to_string(),
                        api_url: "".to_string(),
                        base_url: "https://api.openai.com".to_string(),
                        api_key: "test-key".to_string(),
                        model: "gpt-4o".to_string(),
                        temperature: 0.7,
                        max_tokens: 4096,
                        prompt_cache_enabled: false,
                        unknown: Default::default(),
                    },
                );
                map
            },
            tools: vec![],
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                cron: crate::config::CronConfig { enabled: false },
                heartbeat: crate::config::HeartbeatConfig {
                    enabled: false,
                    interval_s: 60,
                },
            },
        });

        let agent_loop =
            AgentLoop::from_config(&paths, message_bus, _receivers.inbound, config.clone())
                .await
                .unwrap();
        assert_eq!(agent_loop.config().agent.provider, "default");
    }

    #[tokio::test]
    async fn test_agent_loop_register_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(&tmp);
        let (message_bus, _receivers) = MessageBus::new();
        let config = Arc::new(crate::config::Config {
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                max_tool_result_chars: 8000,
                persist_tool_results: false,
                context_window_tokens: 32768,
                unknown: Default::default(),
            },
            providers: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "default".to_string(),
                    crate::config::ProviderConfig {
                        r#type: "openai".to_string(),
                        api_url: "".to_string(),
                        base_url: "https://api.openai.com".to_string(),
                        api_key: "test-key".to_string(),
                        model: "gpt-4o".to_string(),
                        temperature: 0.7,
                        max_tokens: 4096,
                        prompt_cache_enabled: false,
                        unknown: Default::default(),
                    },
                );
                map
            },
            tools: vec![],
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                cron: crate::config::CronConfig { enabled: false },
                heartbeat: crate::config::HeartbeatConfig {
                    enabled: false,
                    interval_s: 60,
                },
            },
        });

        let mut agent_loop =
            AgentLoop::from_config(&paths, message_bus, _receivers.inbound, config)
                .await
                .unwrap();
        // register_tool is a no-op but should not panic
        agent_loop.register_tool(Box::new(crate::tools::shell::ShellTool::default()));
    }

    #[tokio::test]
    async fn test_shutdown_handle_creation() {
        let (shutdown_tx, _) = broadcast::channel::<()>(1);
        let (inbound_tx, _) = mpsc::channel(10);

        let handle = ShutdownHandle {
            shutdown_tx,
            inbound_tx,
        };

        // Verify handle was created
        assert_eq!(handle.shutdown_tx.receiver_count(), 0);
    }

    async fn make_agent_loop(tmp: &tempfile::TempDir) -> AgentLoop {
        let paths = make_test_paths(tmp);
        let (message_bus, _receivers) = MessageBus::new();
        let config = Arc::new(crate::config::Config {
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                max_tool_result_chars: 8000,
                persist_tool_results: false,
                context_window_tokens: 32768,
                unknown: Default::default(),
            },
            providers: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "default".to_string(),
                    crate::config::ProviderConfig {
                        r#type: "openai".to_string(),
                        api_url: "".to_string(),
                        base_url: "https://api.openai.com".to_string(),
                        api_key: "test-key".to_string(),
                        model: "gpt-4o".to_string(),
                        temperature: 0.7,
                        max_tokens: 4096,
                        prompt_cache_enabled: false,
                        unknown: Default::default(),
                    },
                );
                map
            },
            tools: vec![],
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                cron: crate::config::CronConfig { enabled: false },
                heartbeat: crate::config::HeartbeatConfig {
                    enabled: false,
                    interval_s: 60,
                },
            },
        });
        AgentLoop::from_config(&paths, message_bus, _receivers.inbound, config)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_agent_loop_shutdown_for_cli() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_loop = make_agent_loop(&tmp).await;
        // shutdown_for_cli should not panic
        agent_loop.shutdown_for_cli().await;
    }

    #[tokio::test]
    async fn test_agent_loop_start_inbound() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_loop = make_agent_loop(&tmp).await;
        // start_inbound spawns a task; should not panic
        agent_loop.start_inbound(None);
        // Give the task time to spawn
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Trigger shutdown to clean up the spawned task
        agent_loop.shutdown_for_cli().await;
    }

    #[tokio::test]
    async fn test_agent_loop_run_task_nonexistent_session() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_loop = make_agent_loop(&tmp).await;
        let hook = TaskHook::new("new-session");
        // run_task should create the session and run
        let result = agent_loop
            .run_task("new-session", "test".to_string(), hook, None, None, None)
            .await;
        // Result should be returned (success depends on provider, but session should be created)
        let sm = agent_loop.session_manager();
        assert!(sm.lock().await.has_session("new-session"));
        let _ = result;
    }

    #[tokio::test]
    async fn test_agent_loop_run_returns_shutdown_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_loop = Arc::new(make_agent_loop(&tmp).await);

        let (message_bus, _receivers) = MessageBus::new();
        let config = Arc::new(crate::config::Config {
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                max_tool_result_chars: 8000,
                persist_tool_results: false,
                context_window_tokens: 32768,
                unknown: Default::default(),
            },
            providers: std::collections::HashMap::new(),
            tools: vec![],
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                cron: crate::config::CronConfig { enabled: false },
                heartbeat: crate::config::HeartbeatConfig {
                    enabled: false,
                    interval_s: 60,
                },
            },
        });
        let (event_tx, _) = broadcast::channel::<crate::session::AgentEvent>(256);
        let channel_manager = Arc::new(crate::channel::ChannelManager::new(
            message_bus,
            _receivers.outbound,
            config,
            Some(event_tx),
        ));

        let handle = agent_loop.run(&channel_manager);
        // Handle should have a valid shutdown_tx
        assert!(handle.shutdown_tx.receiver_count() >= 1);

        // Trigger shutdown to clean up
        let _ = handle.shutdown_tx.send(());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_agent_loop_session_manager_accessor() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_loop = make_agent_loop(&tmp).await;
        let sm = agent_loop.session_manager();
        // Should be usable
        sm.lock().await.get_or_create("test").await.unwrap();
        assert!(sm.lock().await.has_session("test"));
    }

    #[tokio::test]
    async fn test_agent_loop_config_accessor() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_loop = make_agent_loop(&tmp).await;
        let config = agent_loop.config();
        assert_eq!(config.agent.provider, "default");
        assert_eq!(config.agent.max_iterations, 10);
    }

    #[tokio::test]
    async fn test_agent_loop_from_config_missing_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(&tmp);
        let (message_bus, _receivers) = MessageBus::new();
        let config = Arc::new(crate::config::Config {
            agent: crate::config::AgentConfig {
                provider: "nonexistent".to_string(), // Provider doesn't exist
                max_iterations: 10,
                timeout_seconds: 30,
                max_tool_result_chars: 8000,
                persist_tool_results: false,
                context_window_tokens: 32768,
                unknown: Default::default(),
            },
            providers: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "default".to_string(),
                    crate::config::ProviderConfig {
                        r#type: "openai".to_string(),
                        api_url: "".to_string(),
                        base_url: "https://api.openai.com".to_string(),
                        api_key: "test-key".to_string(),
                        model: "gpt-4o".to_string(),
                        temperature: 0.7,
                        max_tokens: 4096,
                        prompt_cache_enabled: false,
                        unknown: Default::default(),
                    },
                );
                map
            },
            tools: vec![],
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                cron: crate::config::CronConfig { enabled: false },
                heartbeat: crate::config::HeartbeatConfig {
                    enabled: false,
                    interval_s: 60,
                },
            },
        });

        let result = AgentLoop::from_config(&paths, message_bus, _receivers.inbound, config).await;
        assert!(result.is_err());
        let err_str = format!("{}", result.err().unwrap());
        assert!(err_str.contains("not found"));
    }

    #[tokio::test]
    async fn test_handle_agent_loop_command_status() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));
        let (outbound_tx, _) = mpsc::channel(10);

        // Create a session
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("test-session").await.unwrap();
            guard
                .add_message("test-session", Message::user("hello".to_string()))
                .await
                .unwrap();
        }

        // Test /status command
        let (response, should_shutdown) =
            AgentLoop::handle_agent_loop_command(&sm, &outbound_tx, "test-session", "/status")
                .await;
        assert!(!should_shutdown);
        assert!(response.contains("Session: test-session"));
        assert!(response.contains("Messages: 1"));
    }

    #[tokio::test]
    async fn test_handle_agent_loop_command_clear() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir.clone()).unwrap(),
        ));
        let (outbound_tx, _) = mpsc::channel(10);

        // Create a session with messages
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("test-session").await.unwrap();
            guard
                .add_message("test-session", Message::user("hello".to_string()))
                .await
                .unwrap();
            guard.persist("test-session").await.unwrap();
        }

        // Verify session has messages
        assert!(session_dir.join("test-session.jsonl").exists());

        // Test /clear command
        let (response, should_shutdown) =
            AgentLoop::handle_agent_loop_command(&sm, &outbound_tx, "test-session", "/clear").await;
        assert!(!should_shutdown);
        assert!(response.contains("cleared"));

        // Verify files were deleted
        assert!(!session_dir.join("test-session.jsonl").exists());
    }

    #[tokio::test]
    async fn test_handle_agent_loop_command_stop_no_session() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));
        let (outbound_tx, _) = mpsc::channel(10);

        // Test /stop command with no session
        let (response, should_shutdown) =
            AgentLoop::handle_agent_loop_command(&sm, &outbound_tx, "nonexistent-session", "/stop")
                .await;
        assert!(!should_shutdown);
        assert!(response.contains("No session to stop"));
    }

    #[tokio::test]
    async fn test_handle_agent_loop_command_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));
        let (outbound_tx, _) = mpsc::channel(10);

        // Test unknown command
        let (response, should_shutdown) =
            AgentLoop::handle_agent_loop_command(&sm, &outbound_tx, "test-session", "/unknown")
                .await;
        assert!(!should_shutdown);
        assert!(response.is_empty());
    }
}
