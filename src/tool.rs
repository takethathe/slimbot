use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::ToolEntry;
use crate::utils::{
    TOOL_RESULT_PREVIEW_CHARS, TOOL_RESULTS_DIR, build_persisted_reference, write_file_atomic,
};
use crate::{debug, error, warn_log};

// Re-export for runner.rs
pub use crate::utils::truncate_text_head_tail;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// Channel and chat_id context injected into tools at the start of each turn.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub channel: String,
    pub chat_id: String,
}

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;
    async fn execute(&self, args: serde_json::Value) -> Result<String>;

    /// Set the current session context. Default is no-op.
    fn set_context(&self, _ctx: &ToolContext) {}
    /// Start a new turn (reset per-turn tracking). Default is no-op.
    fn start_turn(&self) {}
    /// Check if this tool sent output this turn. Default is false.
    fn sent_in_turn(&self) -> bool {
        false
    }
}

pub struct ToolManager {
    tools: HashMap<String, Box<dyn Tool>>,
    workspace_dir: PathBuf,
    /// Cached OpenAI tool definitions, rebuilt on register.
    openai_tools: Vec<ToolDefinition>,
}

impl ToolManager {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self {
            tools: HashMap::new(),
            workspace_dir,
            openai_tools: Vec::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        debug!("Registered tool: {}", name);
        self.openai_tools.push(ToolDefinition {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            parameters: tool.parameters(),
        });
        self.tools.insert(name, tool);
    }

    pub fn init_from_config(&mut self, entries: &[ToolEntry]) {
        // If no tools configured in config, enable all built-in tools by default
        let default_entries = vec![
            ToolEntry {
                name: "shell".into(),
                enabled: true,
            },
            ToolEntry {
                name: "file_reader".into(),
                enabled: true,
            },
            ToolEntry {
                name: "file_writer".into(),
                enabled: true,
            },
            ToolEntry {
                name: "file_editor".into(),
                enabled: true,
            },
            ToolEntry {
                name: "list_dir".into(),
                enabled: true,
            },
            ToolEntry {
                name: "make_dir".into(),
                enabled: true,
            },
            ToolEntry {
                name: "grep".into(),
                enabled: true,
            },
            ToolEntry {
                name: "find_files".into(),
                enabled: true,
            },
        ];
        let effective = if entries.is_empty() {
            &default_entries
        } else {
            entries
        };

        for entry in effective {
            if entry.enabled {
                if let Some(tool) = create_builtin_tool(&entry.name, &self.workspace_dir) {
                    self.register(tool);
                } else {
                    warn_log!("Unknown tool: {}", entry.name);
                }
            }
        }
    }

    pub fn to_openai_functions(&self) -> Vec<ToolDefinition> {
        self.openai_tools.clone()
    }

    pub async fn execute(&self, name: &str, args: serde_json::Value) -> Result<String> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Tool not found: {}", name))?;
        tool.execute(args).await
    }

    /// Inject session context into all tools that support it.
    pub fn set_context(&self, ctx: &ToolContext) {
        for tool in self.tools.values() {
            tool.set_context(ctx);
        }
    }

    /// Start a new turn for a named tool (resets per-turn tracking).
    pub fn start_turn(&self, name: &str) {
        if let Some(tool) = self.tools.get(name) {
            tool.start_turn();
        }
    }

    /// Check if a named tool sent a message this turn.
    pub fn sent_in_turn(&self, name: &str) -> bool {
        self.tools.get(name).is_some_and(|t| t.sent_in_turn())
    }
}

fn create_builtin_tool(name: &str, workspace_dir: &Path) -> Option<Box<dyn Tool>> {
    crate::tools::create_tool(name, workspace_dir)
}

/// Replace semantically empty tool results with a short marker string.
pub fn ensure_nonempty_tool_result(tool_name: &str, content: &str) -> String {
    if content.is_empty() || content.trim().is_empty() {
        format!("({} completed with no output)", tool_name)
    } else {
        content.to_string()
    }
}

/// Format a tool execution error into a model-friendly message.
pub fn format_tool_error(error_msg: &str) -> String {
    format!(
        "Error: {}\n\n[Analyze the error above and try a different approach.]",
        error_msg,
    )
}

/// Persist oversized tool result to disk and return a reference string.
pub fn persist_tool_result(
    workspace_dir: &Path,
    tool_call_id: &str,
    content: &str,
    max_chars: usize,
) -> String {
    if content.len() <= max_chars {
        return content.to_string();
    }

    let results_dir = workspace_dir.join(TOOL_RESULTS_DIR);
    if let Err(e) = fs::create_dir_all(&results_dir) {
        error!("Failed to create tool-results dir: {}", e);
        return content.to_string();
    }

    let file_path = results_dir.join(format!("{}.txt", tool_call_id));
    if !file_path.exists()
        && let Err(e) = write_file_atomic(&file_path, content)
    {
        error!("Failed to persist tool result: {}", e);
        return content.to_string();
    }

    build_persisted_reference(&file_path, content, TOOL_RESULT_PREVIEW_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolEntry;

    #[test]
    fn test_tool_manager_new_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let tm = ToolManager::new(tmp.path().to_path_buf());
        assert!(tm.to_openai_functions().is_empty());
    }

    #[test]
    fn test_tool_manager_register_and_execute() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tm = ToolManager::new(tmp.path().to_path_buf());
        tm.register(Box::new(crate::tools::shell::ShellTool::default()));

        let funcs = tm.to_openai_functions();
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "shell");
        assert!(!funcs[0].description.is_empty());
    }

    #[test]
    fn test_tool_manager_execute_unknown_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let tm = ToolManager::new(tmp.path().to_path_buf());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(tm.execute("nonexistent", serde_json::json!({})));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Tool not found"));
    }

    #[test]
    fn test_tool_manager_init_from_config_all_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tm = ToolManager::new(tmp.path().to_path_buf());
        tm.init_from_config(&[]); // empty config → all defaults

        let funcs = tm.to_openai_functions();
        assert!(!funcs.is_empty());
        // All 8 built-in tools should be registered
        let names: Vec<_> = funcs.iter().map(|f| &f.name).collect();
        assert!(names.contains(&&"shell".to_string()));
        assert!(names.contains(&&"file_reader".to_string()));
        assert!(names.contains(&&"file_writer".to_string()));
        assert!(names.contains(&&"file_editor".to_string()));
        assert!(names.contains(&&"list_dir".to_string()));
        assert!(names.contains(&&"make_dir".to_string()));
        assert!(names.contains(&&"grep".to_string()));
        assert!(names.contains(&&"find_files".to_string()));
    }

    #[test]
    fn test_tool_manager_init_from_config_selective() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tm = ToolManager::new(tmp.path().to_path_buf());
        tm.init_from_config(&[
            ToolEntry {
                name: "shell".into(),
                enabled: true,
            },
            ToolEntry {
                name: "file_reader".into(),
                enabled: false,
            },
        ]);

        let funcs = tm.to_openai_functions();
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "shell");
    }

    #[test]
    fn test_tool_manager_init_from_config_unknown_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tm = ToolManager::new(tmp.path().to_path_buf());
        tm.init_from_config(&[ToolEntry {
            name: "nonexistent_tool".into(),
            enabled: true,
        }]);

        // Should be empty since unknown tool is skipped
        assert!(tm.to_openai_functions().is_empty());
    }

    #[test]
    fn test_tool_manager_set_context() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tm = ToolManager::new(tmp.path().to_path_buf());
        tm.register(Box::new(crate::tools::shell::ShellTool::default()));

        let ctx = ToolContext {
            channel: "cli".into(),
            chat_id: "test".into(),
        };
        tm.set_context(&ctx);
        // ShellTool doesn't override set_context, so this should be a no-op
    }

    #[test]
    fn test_tool_manager_start_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tm = ToolManager::new(tmp.path().to_path_buf());
        tm.register(Box::new(crate::tools::shell::ShellTool::default()));

        tm.start_turn("shell");
        tm.start_turn("nonexistent"); // no-op for nonexistent tools
    }

    #[test]
    fn test_tool_manager_sent_in_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tm = ToolManager::new(tmp.path().to_path_buf());
        tm.register(Box::new(crate::tools::shell::ShellTool::default()));

        assert!(!tm.sent_in_turn("shell"));
        assert!(!tm.sent_in_turn("nonexistent"));
    }

    #[test]
    fn test_ensure_nonempty_tool_result() {
        assert_eq!(
            ensure_nonempty_tool_result("test_tool", ""),
            "(test_tool completed with no output)"
        );
        assert_eq!(
            ensure_nonempty_tool_result("test_tool", "   "),
            "(test_tool completed with no output)"
        );
        assert_eq!(ensure_nonempty_tool_result("test_tool", "hello"), "hello");
    }

    #[test]
    fn test_format_tool_error() {
        let formatted = format_tool_error("file not found");
        assert!(formatted.contains("file not found"));
        assert!(formatted.contains("Analyze the error"));
    }

    #[test]
    fn test_persist_tool_result_under_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let result = persist_tool_result(tmp.path(), "call-1", "short result", 100);
        assert_eq!(result, "short result");
    }

    #[test]
    fn test_persist_tool_result_over_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let content = "x".repeat(2000); // well over the 1200 preview limit
        let result = persist_tool_result(tmp.path(), "call-2", &content, 100);
        assert!(result.contains("persisted"));
        assert!(result.contains("call-2"));
        assert!(result.contains("Original size"));
    }

    #[test]
    fn test_create_builtin_tool_known_names() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(create_builtin_tool("shell", tmp.path()).is_some());
        assert!(create_builtin_tool("file_reader", tmp.path()).is_some());
        assert!(create_builtin_tool("file_writer", tmp.path()).is_some());
        assert!(create_builtin_tool("file_editor", tmp.path()).is_some());
        assert!(create_builtin_tool("list_dir", tmp.path()).is_some());
        assert!(create_builtin_tool("make_dir", tmp.path()).is_some());
        assert!(create_builtin_tool("grep", tmp.path()).is_some());
        assert!(create_builtin_tool("find_files", tmp.path()).is_some());
        assert!(create_builtin_tool("unknown_tool", tmp.path()).is_none());
    }
}
