use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;
use crate::context::ContextBuilder;
use crate::memory::MemoryStore;
use crate::message_bus::{BusRequest, BusResult, MessageBus};
use crate::path::PathManager;
use crate::provider::{OpenAIProvider, Provider};
use crate::runner::{AgentResult, AgentRunner};
use crate::session::{SessionManager, SessionTask, SharedSessionManager, ensure_session};
use crate::tool::{Tool, ToolManager};

pub struct AgentLoop {
    config: Config,
    workspace_dir: PathBuf,
    provider: Arc<dyn Provider>,
    tool_manager: Arc<ToolManager>,
    session_manager: SharedSessionManager,
    memory_store: Arc<MemoryStore>,
    message_bus: Arc<MessageBus>,
}

impl AgentLoop {
    pub async fn from_config(paths: &PathManager, message_bus: Arc<MessageBus>) -> Result<Self> {
        let config = Config::load(paths.config_path().to_str().unwrap())?;

        let provider_config = config
            .providers
            .get(&config.agent.provider)
            .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found", config.agent.provider))?;
        let provider = Arc::new(OpenAIProvider::new(provider_config));

        let mut tool_manager = ToolManager::new(paths.workspace_dir().to_path_buf());
        tool_manager.init_from_config(&config.tools);

        let session_manager =
            Arc::new(tokio::sync::Mutex::new(SessionManager::new(paths.session_dir())?));

        let memory_store = Arc::new(MemoryStore::new(paths.workspace_dir()));
        memory_store.init()?;

        Ok(Self {
            config,
            workspace_dir: paths.workspace_dir().to_path_buf(),
            provider,
            tool_manager: Arc::new(tool_manager),
            session_manager,
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

    pub async fn run_task(
        &self,
        session_id: &str,
        task: &mut SessionTask,
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
        runner.run(task, session_id).await
    }

    /// Start the inbound listener task. Drains inbound_rx, runs AgentLoop,
    /// publishes results to outbound_tx. This is the main processing loop.
    pub fn start_inbound(&self) {
        let inbound_rx = self.message_bus.inbound_rx();
        let outbound_tx = self.message_bus.outbound_tx();
        let session_manager = self.session_manager.clone();
        let tool_manager = self.tool_manager.clone();
        let provider = self.provider.clone();
        let config = self.config.agent.clone();
        let workspace_dir = self.workspace_dir.clone();
        let memory_store = self.memory_store.clone();

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

                let task_id = uuid::Uuid::new_v4().to_string();
                let hook = request.hook.clone();
                let task = SessionTask {
                    id: task_id.clone(),
                    content: request.content,
                    hook: request.hook,
                    state: crate::session::TaskState::Pending,
                };

                {
                    let mut guard = session_manager.lock().await;
                    guard.enqueue_task(&request.session_id, task).await;
                }

                let mut task = {
                    let mut guard = session_manager.lock().await;
                    guard.dequeue_task(&request.session_id).await
                }
                .unwrap_or_else(|| SessionTask {
                    id: task_id.clone(),
                    content: String::new(),
                    hook: hook.clone(),
                    state: crate::session::TaskState::Failed {
                        error: "Queue empty".to_string(),
                    },
                });

                let runner = AgentRunner::new(
                    ContextBuilder::new(
                        session_manager.clone(),
                        tool_manager.clone(),
                        workspace_dir.clone(),
                        memory_store.clone(),
                    ),
                    tool_manager.clone(),
                    provider.clone(),
                    session_manager.clone(),
                    config.clone(),
                    workspace_dir.clone(),
                    request.channel_inject,
                );
                let agent_result = runner.run(&mut task, &request.session_id).await;

                let content = if agent_result.success {
                    agent_result.content
                } else {
                    format!("Error: {}", agent_result.content)
                };

                let _ = outbound_tx
                    .send(BusResult {
                        session_id: request.session_id,
                        task_id: task.id,
                        content,
                    })
                    .await;
            }
        });
    }
}
