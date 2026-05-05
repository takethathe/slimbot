use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::ToolEntry;
use crate::utils::{
    TOOL_RESULTS_DIR, TOOL_RESULT_PREVIEW_CHARS, build_persisted_reference,
    write_file_atomic,
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
    fn sent_in_turn(&self) -> bool { false }
}

pub struct ToolManager {
    tools: HashMap<String, Box<dyn Tool>>,
    workspace_dir: PathBuf,
}

impl ToolManager {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self {
            tools: HashMap::new(),
            workspace_dir,
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        debug!("Registered tool: {}", name);
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
        self.tools
            .values()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters(),
            })
            .collect()
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
        self.tools.get(name).map_or(false, |t| t.sent_in_turn())
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
    if !file_path.exists() {
        if let Err(e) = write_file_atomic(&file_path, content) {
            error!("Failed to persist tool result: {}", e);
            return content.to_string();
        }
    }

    build_persisted_reference(&file_path, content, TOOL_RESULT_PREVIEW_CHARS)
}
