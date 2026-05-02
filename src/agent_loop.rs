use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::context::ContextBuilder;
use crate::memory::MemoryStore;
use crate::provider::{OpenAIProvider, Provider};
use crate::runner::{AgentResult, AgentRunner};
use crate::session::{SessionManager, SessionTask, SharedSessionManager};
use crate::tool::{Tool, ToolManager};

pub struct AgentLoop {
    config: Config,
    provider: Arc<dyn Provider>,
    tool_manager: Arc<ToolManager>,
    session_manager: SharedSessionManager,
    memory_store: Arc<MemoryStore>,
}

impl AgentLoop {
    pub async fn from_config(config_path: &str) -> Result<Self> {
        let config = Config::load(config_path)?;

        let provider_config = config
            .providers
            .get(&config.agent.provider)
            .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found", config.agent.provider))?;
        let provider = Arc::new(OpenAIProvider::new(provider_config));

        let mut tool_manager = ToolManager::new(config.workspace_dir());
        tool_manager.init_from_config(&config.tools);

        let session_manager = Arc::new(Mutex::new(SessionManager::new(config.session_dir())?));

        let memory_store = Arc::new(MemoryStore::new(&config.workspace_dir()));
        memory_store.init()?;

        Ok(Self {
            config,
            provider,
            tool_manager: Arc::new(tool_manager),
            session_manager,
            memory_store,
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
                self.config.workspace_dir(),
                self.memory_store.clone(),
            ),
            self.tool_manager.clone(),
            self.provider.clone(),
            self.session_manager.clone(),
            self.config.agent.clone(),
            self.config.workspace_dir(),
            channel_inject,
        );
        runner.run(task, session_id).await
    }
}
