use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::ToolEntry;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
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
}

pub struct ToolManager {
    tools: HashMap<String, Box<dyn Tool>>,
    data_dir: PathBuf,
}

impl ToolManager {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            tools: HashMap::new(),
            data_dir,
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        eprintln!("Registered tool: {}", name);
        self.tools.insert(name, tool);
    }

    pub fn init_from_config(&mut self, entries: &[ToolEntry]) {
        // If no tools configured in config, enable all built-in tools by default
        let default_entries = vec![
            ToolEntry { name: "shell".into(), enabled: true },
            ToolEntry { name: "file_reader".into(), enabled: true },
            ToolEntry { name: "file_writer".into(), enabled: true },
            ToolEntry { name: "file_editor".into(), enabled: true },
            ToolEntry { name: "list_dir".into(), enabled: true },
            ToolEntry { name: "make_dir".into(), enabled: true },
        ];
        let effective = if entries.is_empty() { &default_entries } else { entries };

        for entry in effective {
            if entry.enabled {
                if let Some(tool) = create_builtin_tool(&entry.name, &self.data_dir) {
                    self.register(tool);
                } else {
                    eprintln!("Unknown tool: {}", entry.name);
                }
            }
        }
    }

    pub fn to_openai_functions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|tool| {
            ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters(),
            }
        }).collect()
    }

    pub async fn execute(&self, name: &str, args: serde_json::Value) -> Result<String> {
        let tool = self.tools.get(name)
            .ok_or_else(|| anyhow::anyhow!("Tool not found: {}", name))?;
        tool.execute(args).await
    }
}

fn create_builtin_tool(name: &str, data_dir: &Path) -> Option<Box<dyn Tool>> {
    crate::tools::create_tool(name, data_dir)
}
