use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::DreamConfig;
use crate::memory::SharedMemory;
use crate::provider::Provider;
use crate::session::SharedSessionManager;
use crate::tool::ToolManager;
use crate::tools::memory_tools::{MemoryEditTool, MemoryReadTool, MemoryWriteTool};

pub struct DreamService {
    workspace_dir: PathBuf,
    memory_service: SharedMemory,
    session_manager: SharedSessionManager,
    config: DreamConfig,
    providers: Arc<HashMap<String, Arc<dyn Provider>>>,
    default_provider: Arc<dyn Provider>,
}

pub struct DreamResult {
    pub success: bool,
    pub elapsed_ms: i64,
    pub message: String,
}

impl DreamService {
    pub fn new(
        workspace_dir: PathBuf,
        memory_service: SharedMemory,
        session_manager: SharedSessionManager,
        config: DreamConfig,
        providers: Arc<HashMap<String, Arc<dyn Provider>>>,
        default_provider: Arc<dyn Provider>,
    ) -> Self {
        Self {
            workspace_dir,
            memory_service,
            session_manager,
            config,
            providers,
            default_provider,
        }
    }

    pub async fn build_prompt(&self) -> Option<(String, usize)> {
        let mut ms = self.memory_service.lock().await;
        let dream_cursor = ms.get_last_dream_cursor();
        let entries = ms.read_unprocessed_history(dream_cursor);

        if entries.is_empty() {
            return None;
        }

        let batch = if entries.len() > self.config.max_entries {
            &entries[0..self.config.max_entries]
        } else {
            &entries
        };

        let lines_processed = dream_cursor + batch.len();

        let history_text = batch
            .iter()
            .map(|e| {
                let content = if e.content.len() > 500 {
                    &e.content[..500]
                } else {
                    &e.content
                };
                format!("[{}] {}", e.timestamp, content)
            })
            .collect::<Vec<_>>()
            .join("\n");

        drop(ms);

        let template = crate::embed::get_content("dream.md")?;
        let prompt = template.replace("{history_text}", &history_text);

        Some((prompt, lines_processed))
    }

    /// Build the Dream sandbox tool set: only the memory tools, so Dream can
    /// touch long-term memory exclusively through the `MemoryService` facade.
    /// Existing skill names are injected into each tool description.
    pub async fn build_tools(&self) -> ToolManager {
        let mut tm = ToolManager::new(self.workspace_dir.clone());

        let skill_names = self
            .memory_service
            .lock()
            .await
            .list_skill_names()
            .await
            .unwrap_or_default();

        tm.register(Box::new(MemoryReadTool::new(
            self.memory_service.clone(),
            &skill_names,
        )));
        tm.register(Box::new(MemoryWriteTool::new(
            self.memory_service.clone(),
            &skill_names,
        )));
        tm.register(Box::new(MemoryEditTool::new(
            self.memory_service.clone(),
            &skill_names,
        )));

        tm
    }

    pub fn build_provider(&self) -> Arc<dyn Provider> {
        if let Some(ref model_name) = self.config.model_override {
            if let Some(provider) = self.providers.get(model_name) {
                return provider.clone();
            }
            crate::warn_log!(
                "[DreamService] model_override '{}' not found, using default provider",
                model_name
            );
        }
        self.default_provider.clone()
    }

    pub async fn run(&self) -> DreamResult {
        let start = std::time::Instant::now();

        let (prompt, lines_processed) = match self.build_prompt().await {
            Some(result) => result,
            None => {
                return DreamResult {
                    success: true,
                    elapsed_ms: start.elapsed().as_millis() as i64,
                    message: "Dream: nothing to process".to_string(),
                };
            }
        };

        let provider = self.build_provider();
        let tools = self.build_tools().await;
        let tool_manager = Arc::new(tools);

        // Use the same agent config as the main agent
        let agent_config = crate::config::AgentConfig::default();

        let runner = crate::runner::AgentRunner::new(
            tool_manager,
            provider,
            self.session_manager.clone(),
            agent_config,
            self.workspace_dir.clone(),
            self.memory_service.clone(),
            None, // No consolidator for dream
            4096,
            None, // Dream runs are not traced (out of scope)
        );

        let hook = crate::session::TaskHook::new("system:dream");
        let result = runner
            .run(
                prompt,
                hook,
                "system:dream",
                None,
                Some(tokio_util::sync::CancellationToken::new()),
                None,
                None,
            )
            .await;

        let elapsed_ms = start.elapsed().as_millis() as i64;

        // Check if dream completed successfully
        // Success: LLM returned without tool_calls (normal completion) or finish_reason is Stop
        let success = result.success
            && !result.content.to_lowercase().contains("error")
            && !result.content.to_lowercase().contains("failed");

        if success {
            // Advance dream cursor
            let mut ms = self.memory_service.lock().await;
            if let Err(e) = ms.set_last_dream_cursor(lines_processed) {
                crate::error!("[DreamService] Failed to set dream cursor: {}", e);
            }
            drop(ms);

            // Compact history
            self.compact_history_after_dream().await;

            DreamResult {
                success: true,
                elapsed_ms,
                message: "Dream completed successfully".to_string(),
            }
        } else {
            DreamResult {
                success: false,
                elapsed_ms,
                message: format!("Dream did not complete: {}", result.content),
            }
        }
    }

    async fn compact_history_after_dream(&self) {
        let mut ms = self.memory_service.lock().await;
        if let Err(e) = ms.compact_history_after_dream() {
            crate::error!("[DreamService] Failed to compact history: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::{TempDir, tempdir};

    use crate::memory::MemoryService;

    /// Map of provider name to provider instance.
    type ProviderMap = Arc<HashMap<String, Arc<dyn Provider>>>;

    async fn make_memory_service(workspace_dir: &std::path::Path) -> SharedMemory {
        let memory_service = Arc::new(tokio::sync::Mutex::new(MemoryService::new(workspace_dir)));
        memory_service
            .lock()
            .await
            .init()
            .expect("Failed to init memory store");
        memory_service
    }

    fn make_session_manager(workspace_dir: &std::path::Path) -> SharedSessionManager {
        Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(workspace_dir.join("sessions")).unwrap(),
        ))
    }

    fn make_providers() -> (ProviderMap, Arc<dyn Provider>) {
        use crate::config::ProviderConfig;
        use crate::provider::OpenAIProvider;

        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        let config = ProviderConfig::default();
        providers.insert(
            "default".to_string(),
            Arc::new(OpenAIProvider::new(&config)),
        );

        let default_provider: Arc<dyn Provider> = providers.get("default").unwrap().clone();
        (Arc::new(providers), default_provider)
    }

    async fn make_service() -> (DreamService, TempDir) {
        let tmp_dir = tempdir().unwrap();
        let workspace_dir = tmp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_dir).unwrap();

        let memory_service = make_memory_service(&workspace_dir).await;
        let session_manager = make_session_manager(&workspace_dir);
        let (providers, default_provider) = make_providers();

        let config = DreamConfig::default();

        let service = DreamService::new(
            workspace_dir,
            memory_service,
            session_manager,
            config,
            providers,
            default_provider,
        );

        (service, tmp_dir)
    }

    #[tokio::test]
    async fn test_build_prompt_no_entries_returns_none() {
        let (service, _tmp_dir) = make_service().await;

        let result = service.build_prompt().await;

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_build_prompt_with_entries_formats_correctly() {
        let (service, _tmp_dir) = make_service().await;

        // Set dream cursor to 0 so we can process new entries
        service
            .memory_service
            .lock()
            .await
            .set_last_dream_cursor(0)
            .unwrap();

        // Append entries directly using the memory store
        let mut ms = service.memory_service.lock().await;
        ms.append_history("First entry content").unwrap();
        ms.append_history("Second entry content").unwrap();
        drop(ms);

        let result = service.build_prompt().await;
        assert!(result.is_some());

        let (prompt, lines_processed) = result.unwrap();
        assert!(prompt.contains("["));
        assert!(prompt.contains("First entry content"));
        assert!(prompt.contains("Second entry content"));
        assert_eq!(lines_processed, 2);
    }

    #[tokio::test]
    async fn test_build_prompt_truncates_content_over_500_chars() {
        let (service, _tmp_dir) = make_service().await;

        // Set dream cursor to 0 so we can process new entries
        service
            .memory_service
            .lock()
            .await
            .set_last_dream_cursor(0)
            .unwrap();

        let long_content = "a".repeat(600);
        service
            .memory_service
            .lock()
            .await
            .append_history(&long_content)
            .unwrap();

        let result = service.build_prompt().await;
        assert!(result.is_some());

        let (prompt, _) = result.unwrap();
        // Verify that the history text contains truncated content
        // Each entry is formatted as "[timestamp] content" where content is truncated to 500 chars
        // Check that the prompt contains exactly 500 'a' chars, not 501 or 600
        assert!(prompt.contains(&long_content[..500]));
        assert!(!prompt.contains(&long_content[..501]));
    }

    #[tokio::test]
    async fn test_build_prompt_respects_max_entries() {
        let (mut service, _tmp_dir) = make_service().await;
        // Override max_entries for this test
        service.config.max_entries = 2;

        // Set dream cursor to 0 so we can process new entries
        service
            .memory_service
            .lock()
            .await
            .set_last_dream_cursor(0)
            .unwrap();

        for i in 1..=5 {
            service
                .memory_service
                .lock()
                .await
                .append_history(&format!("Entry {}", i))
                .unwrap();
        }

        let result = service.build_prompt().await;
        assert!(result.is_some());

        let (prompt, lines_processed) = result.unwrap();
        assert!(prompt.contains("Entry 1"));
        assert!(prompt.contains("Entry 2"));
        assert!(!prompt.contains("Entry 3"));
        assert_eq!(lines_processed, 2);
    }

    #[tokio::test]
    async fn test_build_tools_memory_only() {
        let (service, _tmp_dir) = make_service().await;

        let tools = service.build_tools().await;
        let tool_defs = tools.to_openai_functions();
        let tool_names: Vec<_> = tool_defs.iter().map(|t| t.name.as_str()).collect();

        assert!(tool_names.contains(&"memory_read"));
        assert!(tool_names.contains(&"memory_write"));
        assert!(tool_names.contains(&"memory_edit"));
        // No filesystem tools in the Dream sandbox.
        assert!(!tool_names.contains(&"file_reader"));
        assert!(!tool_names.contains(&"file_writer"));
        assert!(!tool_names.contains(&"file_editor"));
        assert!(!tool_names.contains(&"shell"));
        assert!(!tool_names.contains(&"grep"));
        assert_eq!(tool_names.len(), 3);
    }

    #[tokio::test]
    async fn test_build_tools_lists_existing_skills_in_description() {
        let (service, _tmp_dir) = make_service().await;
        service
            .memory_service
            .lock()
            .await
            .set(
                &crate::MemoryCategory::Skill,
                Some("alpha"),
                "---\nname: alpha\ndescription: a\nalways: false\n---\nbody",
            )
            .await
            .unwrap();

        let tools = service.build_tools().await;
        let tool_defs = tools.to_openai_functions();
        let read = tool_defs.iter().find(|t| t.name == "memory_read").unwrap();
        assert!(read.description.contains("Existing skills: alpha."));
    }

    #[tokio::test]
    async fn test_build_provider_without_override() {
        let (service, _tmp_dir) = make_service().await;
        let provider = service.build_provider();
        // Default provider from make_providers() is "default"
        assert!(Arc::ptr_eq(&provider, &service.default_provider));
    }

    #[tokio::test]
    async fn test_build_provider_with_override() {
        let tmp_dir = tempdir().unwrap();
        let workspace_dir = tmp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_dir).unwrap();

        let memory_service = make_memory_service(&workspace_dir).await;
        let session_manager = make_session_manager(&workspace_dir);

        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        let default_config = crate::config::ProviderConfig::default();
        let default_provider: Arc<dyn Provider> =
            Arc::new(crate::provider::OpenAIProvider::new(&default_config));
        providers.insert("default".to_string(), default_provider.clone());

        // Create an override provider with different config
        let override_config = crate::config::ProviderConfig {
            model: "gpt-3.5-turbo".to_string(),
            ..Default::default()
        };
        let override_provider: Arc<dyn Provider> =
            Arc::new(crate::provider::OpenAIProvider::new(&override_config));
        providers.insert("cheap".to_string(), override_provider.clone());

        let config = DreamConfig {
            model_override: Some("cheap".to_string()),
            ..Default::default()
        };

        let service = DreamService::new(
            workspace_dir,
            memory_service,
            session_manager,
            config,
            Arc::new(providers),
            default_provider,
        );

        let provider = service.build_provider();
        // Should return the override provider
        assert!(Arc::ptr_eq(&provider, &override_provider));
    }

    #[tokio::test]
    async fn test_compact_history_keeps_unprocessed_and_recent() {
        let (service, _tmp_dir) = make_service().await;

        {
            let mut ms = service.memory_service.lock().await;
            ms.set_last_dream_cursor(0).unwrap();
            // Create 2100 entries (over HISTORY_MAX_ENTRIES limit of 2000)
            for i in 1..=2100 {
                ms.append_history(&format!("entry {}", i)).unwrap();
            }
            ms.set_last_dream_cursor(2000).unwrap();
        }

        service.compact_history_after_dream().await;

        let mut ms = service.memory_service.lock().await;
        let entries = ms.read_entries();
        // Should compact to DREAM_KEEP_MINIMUM (50)
        assert_eq!(entries.len(), 50);
        // First entry content
        assert_eq!(entries[0].content, "entry 2051");
        // Last entry content
        assert_eq!(entries[49].content, "entry 2100");
        // Dream cursor should be adjusted: 2000 - 2050 = 0 (saturating)
        assert_eq!(ms.get_last_dream_cursor(), 0);
    }
}
