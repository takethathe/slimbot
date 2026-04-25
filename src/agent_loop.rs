use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::context::ContextBuilder;
use crate::provider::{OpenAIProvider, Provider};
use crate::runner::AgentRunner;
use crate::session::{SessionManager, SharedSessionManager, SessionTask};
use crate::tool::{Tool, ToolManager};

pub struct AgentLoop {
    config: Config,
    provider: Arc<dyn Provider>,
    tool_manager: Arc<ToolManager>,
    session_manager: SharedSessionManager,
}

impl AgentLoop {
    pub async fn from_config(config_path: &str) -> Result<Self> {
        let config = Config::load(config_path)?;

        let provider_config = config.providers.get(&config.agent.provider)
            .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found", config.agent.provider))?;
        let provider = Arc::new(OpenAIProvider::new(provider_config));

        let mut tool_manager = ToolManager::new();
        tool_manager.init_from_config(&config.tools);

        let session_manager = Arc::new(Mutex::new(
            SessionManager::new(config.session_dir())?
        ));

        Ok(Self {
            config,
            provider,
            tool_manager: Arc::new(tool_manager),
            session_manager,
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
    ) -> Result<String> {
        let runner = AgentRunner::new(
            ContextBuilder::new(
                self.session_manager.clone(),
                self.tool_manager.clone(),
                Path::new(&self.config.data_dir).join("workspace"),
            ),
            self.tool_manager.clone(),
            self.provider.clone(),
            self.session_manager.clone(),
            self.config.agent.clone(),
        );
        runner.run(task, session_id, channel_inject).await
    }
}
