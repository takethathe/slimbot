use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use crate::tool::Tool;
use crate::tools::resolve_workspace_path;

/// Tool that creates a directory (including parent directories).
pub struct MakeDirTool {
    workspace_dir: PathBuf,
}

impl MakeDirTool {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir }
    }
}

#[async_trait]
impl Tool for MakeDirTool {
    fn name(&self) -> &str {
        "make_dir"
    }

    fn description(&self) -> &str {
        "Create a directory, including all parent directories. Path must be within the workspace directory."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the directory to create (within workspace directory)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let path_str = args["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;

        let target = resolve_workspace_path(path_str, &self.workspace_dir)?;

        tokio::fs::create_dir_all(&target).await
            .map_err(|e| anyhow::anyhow!("Failed to create directory '{}': {}", path_str, e))?;

        Ok(format!("Successfully created directory: {}", path_str))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_make_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = MakeDirTool::new(tmp.path().to_path_buf());
        let result = tool.execute(serde_json::json!({"path": "a/b/c"})).await.unwrap();
        assert!(result.contains("a/b/c"));
        assert!(tmp.path().join("a/b/c").is_dir());
    }

    #[tokio::test]
    async fn test_make_dir_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = MakeDirTool::new(tmp.path().to_path_buf());
        tool.execute(serde_json::json!({"path": "existing"})).await.unwrap();
        // Should not error if directory already exists
        let result = tool.execute(serde_json::json!({"path": "existing"})).await;
        assert!(result.is_ok());
    }
}
