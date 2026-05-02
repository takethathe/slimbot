use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;
use crate::context::ContextBuilder;
use crate::memory::MemoryStore;
use crate::message_bus::{MessageBus};
use crate::path::PathManager;
use crate::provider::{OpenAIProvider, Provider};
use crate::runner::{AgentResult, AgentRunner};
use crate::session::{SessionManager, SessionTaskBuilder, SharedSessionManager, TaskHook, ensure_session};
use crate::tool::{Tool, ToolManager};

pub struct AgentLoop {
    config: Arc<Config>,
    workspace_dir: PathBuf,
    provider: Arc<dyn Provider>,
    tool_manager: Arc<ToolManager>,
    session_manager: SharedSessionManager,
    memory_store: Arc<MemoryStore>,
    message_bus: Arc<MessageBus>,
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

        let session_manager = SessionManager::new(paths.session_dir())?;
        let session_manager_arc = Arc::new(tokio::sync::Mutex::new(session_manager));

        let memory_store = Arc::new(MemoryStore::new(paths.workspace_dir()));
        memory_store.init()?;

        Ok(Self {
            config,
            workspace_dir: paths.workspace_dir().to_path_buf(),
            provider,
            tool_manager: Arc::new(tool_manager),
            session_manager: session_manager_arc,
            memory_store,
            message_bus,
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
        let runner = AgentRunner::new(
            ContextBuilder::new(
                self.session_manager.clone(),
                self.tool_manager.clone(),
                self.workspace_dir.clone(),
                self.memory_store.clone(),
            ),
            self.tool_manager.clone(),
            self.provider.clone(),
            self.session_manager.clone(),
            self.config.agent.clone(),
            self.workspace_dir.clone(),
            channel_inject,
        );
        runner.run(content, hook, session_id).await
    }

    /// Start the inbound listener task.
    /// the session's sequential executor, and publishes results via callback.
    pub fn start_inbound(&self) {
        let inbound_rx = self.message_bus.inbound_rx();
        let session_manager = self.session_manager.clone();
        let tool_manager = self.tool_manager.clone();
        let provider = self.provider.clone();
        let config = self.config.agent.clone();
        let workspace_dir = self.workspace_dir.clone();
        let memory_store = self.memory_store.clone();
        let outbound_tx = self.message_bus.outbound_tx();

        tokio::spawn(async move {
            loop {
                let request = {
                    let mut rx_guard = inbound_rx.lock().await;
                    match rx_guard.recv().await {
                        Some(r) => r,
                        None => break, // channel closed
                    }
                };

                if let Err(e) = ensure_session(&session_manager, &request.session_id).await {
                    eprintln!("[AgentLoop] Session error: {}", e);
                    continue;
                }

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
                .build();

                let mut guard = session_manager.lock().await;
                let _ = guard.submit_task(task).await;
            }
        });
    }
}
