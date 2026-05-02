use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::context::ContextBuilder;
use crate::memory::MemoryStore;
use crate::path::PathManager;
use crate::provider::{OpenAIProvider, Provider};
use crate::runner::{AgentResult, AgentRunner};
use crate::session::{SessionManager, SessionTask, SharedSessionManager};
use crate::tool::{Tool, ToolManager};

pub struct AgentLoop {
    config: Config,
    workspace_dir: PathBuf,
    provider: Arc<dyn Provider>,
    tool_manager: Arc<ToolManager>,
    session_manager: SharedSessionManager,
    memory_store: Arc<MemoryStore>,
}

impl AgentLoop {
    pub async fn from_config(paths: &PathManager) -> Result<Self> {
        let config = Config::load(paths.config_path().to_str().unwrap())?;

        let provider_config = config
            .providers
            .get(&config.agent.provider)
            .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found", config.agent.provider))?;
        let provider = Arc::new(OpenAIProvider::new(provider_config));

        let mut tool_manager = ToolManager::new(paths.workspace_dir().to_path_buf());
        tool_manager.init_from_config(&config.tools);

        let session_manager =
            Arc::new(Mutex::new(SessionManager::new(paths.session_dir())?));

        let memory_store = Arc::new(MemoryStore::new(paths.workspace_dir()));
        memory_store.init()?;

        Ok(Self {
            config,
            workspace_dir: paths.workspace_dir().to_path_buf(),
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
}
