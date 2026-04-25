use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use crate::tool::Tool;
use crate::tools::resolve_workspace_path;

/// Tool that reads file contents from the workspace directory.
pub struct FileReaderTool {
    workspace_dir: PathBuf,
    max_bytes: usize,
}

impl FileReaderTool {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir, max_bytes: 50_000 }
    }
}

#[async_trait]
impl Tool for FileReaderTool {
    fn name(&self) -> &str {
        "file_reader"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Path must be within the workspace directory."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read (within workspace directory)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let path_str = args["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;

        let resolved = resolve_workspace_path(path_str, &self.workspace_dir)?;

        if !resolved.is_file() {
            return Err(anyhow::anyhow!("Not a file: {}", path_str));
        }

        let content = tokio::fs::read_to_string(&resolved).await
            .map_err(|e| anyhow::anyhow!("Failed to read file '{}': {}", path_str, e))?;

        // Truncate to avoid context explosion
        if content.len() > self.max_bytes {
            Ok(format!(
                "(truncated to {} chars)\n{}",
                self.max_bytes,
                &content[..self.max_bytes]
            ))
        } else {
            Ok(content)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_read_nonexistent_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = FileReaderTool::new(tmp.path().to_path_buf());
        let result = tool.execute(serde_json::json!({"path": "nonexistent.txt"})).await;
        assert!(result.is_err());
    }
}
