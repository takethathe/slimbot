use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::config::DreamConfig;
use crate::memory::SharedMemoryStore;
use crate::provider::Provider;
use crate::session::SharedSessionManager;
use crate::tool::{Tool, ToolManager};
use crate::tools::resolve_workspace_path;

pub struct DreamService {
    workspace_dir: PathBuf,
    memory_store: SharedMemoryStore,
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

/// Restricted file writer that only allows writing to a specific directory.
struct DreamFileWriterTool {
    workspace_dir: PathBuf,
    allowed_dir: PathBuf,
}

#[async_trait]
impl Tool for DreamFileWriterTool {
    fn name(&self) -> &str {
        "file_writer"
    }

    fn description(&self) -> &str {
        "Write content to a file within the skills directory. Path must be within skills/."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write (must be within skills/ directory)"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: content"))?;

        let path = resolve_workspace_path(path_str, &self.workspace_dir)?;

        // Check if path is within allowed directory
        if !path.starts_with(&self.allowed_dir) {
            anyhow::bail!(
                "Dream file_writer can only write to skills/ directory. Got: {}",
                path_str
            );
        }

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::write(&path, content)?;
        Ok(format!("Successfully wrote to {}", path_str))
    }
}

/// Restricted file editor that only allows editing specific files or directories.
struct DreamFileEditorTool {
    workspace_dir: PathBuf,
    allowed_files: Vec<PathBuf>,
    allowed_dir: PathBuf,
}

#[async_trait]
impl Tool for DreamFileEditorTool {
    fn name(&self) -> &str {
        "file_editor"
    }

    fn description(&self) -> &str {
        "Edit memory files (SOUL.md, USER.md, memory/MEMORY.md) or files in skills/ directory by replacing exact text."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact text to search for and replace (must appear exactly once)"
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement text"
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;
        let old_string = args["old_string"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: old_string"))?;
        let new_string = args["new_string"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: new_string"))?;

        let path = resolve_workspace_path(path_str, &self.workspace_dir)?;

        // Check if path is allowed
        let is_allowed_file = self.allowed_files.iter().any(|f| path == *f);
        let is_in_allowed_dir = path.starts_with(&self.allowed_dir);

        if !is_allowed_file && !is_in_allowed_dir {
            anyhow::bail!(
                "Dream file_editor can only edit SOUL.md, USER.md, memory/MEMORY.md, or files in skills/. Got: {}",
                path_str
            );
        }

        let content = std::fs::read_to_string(&path)?;
        let matches = content.matches(old_string).count();

        if matches == 0 {
            anyhow::bail!("old_string not found in file");
        }
        if matches > 1 {
            anyhow::bail!(
                "old_string appears {} times, must appear exactly once",
                matches
            );
        }

        let new_content = content.replacen(old_string, new_string, 1);
        std::fs::write(&path, &new_content)?;

        Ok(format!("Successfully edited {}", path_str))
    }
}

impl DreamService {
    pub fn new(
        workspace_dir: PathBuf,
        memory_store: SharedMemoryStore,
        session_manager: SharedSessionManager,
        config: DreamConfig,
        providers: Arc<HashMap<String, Arc<dyn Provider>>>,
        default_provider: Arc<dyn Provider>,
    ) -> Self {
        Self {
            workspace_dir,
            memory_store,
            session_manager,
            config,
            providers,
            default_provider,
        }
    }

    pub async fn build_prompt(&self) -> Option<(String, u64)> {
        let mut ms = self.memory_store.lock().await;
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

        let last_cursor = batch.last().map(|e| e.cursor).unwrap_or(0);

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

        Some((prompt, last_cursor))
    }

    pub fn build_tools(&self) -> ToolManager {
        let mut tm = ToolManager::new(self.workspace_dir.clone());

        // file_reader: can read any file in workspace
        tm.register(Box::new(crate::tools::file_reader::FileReaderTool::new(
            self.workspace_dir.clone(),
        )));

        // file_writer: restricted to skills/ directory only
        tm.register(Box::new(DreamFileWriterTool {
            workspace_dir: self.workspace_dir.clone(),
            allowed_dir: self.workspace_dir.join("skills"),
        }));

        // file_editor: restricted to memory files and skills/
        tm.register(Box::new(DreamFileEditorTool {
            workspace_dir: self.workspace_dir.clone(),
            allowed_files: vec![
                self.workspace_dir.join("SOUL.md"),
                self.workspace_dir.join("USER.md"),
                self.workspace_dir.join("memory").join("MEMORY.md"),
            ],
            allowed_dir: self.workspace_dir.join("skills"),
        }));

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

        let (prompt, last_cursor) = match self.build_prompt().await {
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
        let tools = self.build_tools();
        let tool_manager = Arc::new(tools);

        // Use the same agent config as the main agent
        let agent_config = crate::config::AgentConfig::default();

        let runner = crate::runner::AgentRunner::new(
            tool_manager,
            provider,
            self.session_manager.clone(),
            agent_config,
            self.workspace_dir.clone(),
            self.memory_store.clone(),
            None, // No consolidator for dream
            4096,
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
            let mut ms = self.memory_store.lock().await;
            if let Err(e) = ms.set_last_dream_cursor(last_cursor) {
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
        let mut ms = self.memory_store.lock().await;
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

    use crate::memory::MemoryStore;

    async fn make_memory_store(workspace_dir: &std::path::Path) -> SharedMemoryStore {
        let memory_store = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(workspace_dir)));
        memory_store
            .lock()
            .await
            .init()
            .expect("Failed to init memory store");
        memory_store
    }

    fn make_session_manager(workspace_dir: &std::path::Path) -> SharedSessionManager {
        Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(workspace_dir.join("sessions")).unwrap(),
        ))
    }

    fn make_providers() -> (Arc<HashMap<String, Arc<dyn Provider>>>, Arc<dyn Provider>) {
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

        let memory_store = make_memory_store(&workspace_dir).await;
        let session_manager = make_session_manager(&workspace_dir);
        let (providers, default_provider) = make_providers();

        let config = DreamConfig::default();

        let service = DreamService::new(
            workspace_dir,
            memory_store,
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
            .memory_store
            .lock()
            .await
            .set_last_dream_cursor(0)
            .unwrap();

        // Append entries directly using the memory store
        let mut ms = service.memory_store.lock().await;
        ms.append_history("First entry content").unwrap();
        ms.append_history("Second entry content").unwrap();
        drop(ms);

        let result = service.build_prompt().await;
        assert!(result.is_some());

        let (prompt, cursor) = result.unwrap();
        assert!(prompt.contains("["));
        assert!(prompt.contains("First entry content"));
        assert!(prompt.contains("Second entry content"));
        assert_eq!(cursor, 2);
    }

    #[tokio::test]
    async fn test_build_prompt_truncates_content_over_500_chars() {
        let (service, _tmp_dir) = make_service().await;

        // Set dream cursor to 0 so we can process new entries
        service
            .memory_store
            .lock()
            .await
            .set_last_dream_cursor(0)
            .unwrap();

        let long_content = "a".repeat(600);
        service
            .memory_store
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
            .memory_store
            .lock()
            .await
            .set_last_dream_cursor(0)
            .unwrap();

        for i in 1..=5 {
            service
                .memory_store
                .lock()
                .await
                .append_history(&format!("Entry {}", i))
                .unwrap();
        }

        let result = service.build_prompt().await;
        assert!(result.is_some());

        let (prompt, cursor) = result.unwrap();
        assert!(prompt.contains("Entry 1"));
        assert!(prompt.contains("Entry 2"));
        assert!(!prompt.contains("Entry 3"));
        assert_eq!(cursor, 2);
    }

    #[tokio::test]
    async fn test_build_tools_restricted_file_set() {
        let (service, _tmp_dir) = make_service().await;

        let tools = service.build_tools();
        let tool_defs = tools.to_openai_functions();
        let tool_names: Vec<_> = tool_defs.iter().map(|t| t.name.as_str()).collect();

        assert!(tool_names.contains(&"file_reader"));
        assert!(tool_names.contains(&"file_writer"));
        assert!(tool_names.contains(&"file_editor"));
        assert!(!tool_names.contains(&"shell"));
        assert!(!tool_names.contains(&"grep"));
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

        let memory_store = make_memory_store(&workspace_dir).await;
        let session_manager = make_session_manager(&workspace_dir);

        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        let default_config = crate::config::ProviderConfig::default();
        let default_provider: Arc<dyn Provider> =
            Arc::new(crate::provider::OpenAIProvider::new(&default_config));
        providers.insert("default".to_string(), default_provider.clone());

        // Create an override provider with different config
        let mut override_config = crate::config::ProviderConfig::default();
        override_config.model = "gpt-3.5-turbo".to_string();
        let override_provider: Arc<dyn Provider> =
            Arc::new(crate::provider::OpenAIProvider::new(&override_config));
        providers.insert("cheap".to_string(), override_provider.clone());

        let config = DreamConfig {
            model_override: Some("cheap".to_string()),
            ..Default::default()
        };

        let service = DreamService::new(
            workspace_dir,
            memory_store,
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
            let mut ms = service.memory_store.lock().await;
            ms.set_last_dream_cursor(0).unwrap();
            // Create 2100 entries (over HISTORY_MAX_ENTRIES limit of 2000)
            for i in 1..=2100 {
                ms.append_history(&format!("entry {}", i)).unwrap();
            }
            ms.set_last_dream_cursor(2000).unwrap();
        }

        service.compact_history_after_dream().await;

        let mut ms = service.memory_store.lock().await;
        let entries = ms.read_entries();
        // Should compact to DREAM_KEEP_MINIMUM (50)
        assert_eq!(entries.len(), 50);
        // First entry should be cursor 2051
        assert_eq!(entries[0].cursor, 2051);
        // Last entry should be cursor 2100
        assert_eq!(entries[49].cursor, 2100);
        // Dream cursor should be repositioned to 2050 (2051 - 1)
        assert_eq!(ms.get_last_dream_cursor(), 2050);
    }
}
