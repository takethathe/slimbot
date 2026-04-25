use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use crate::tool::Tool;
use crate::tools::resolve_workspace_path;

/// Tool that writes content to a file (creates or overwrites).
pub struct FileWriterTool {
    workspace_dir: PathBuf,
}

impl FileWriterTool {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir }
    }
}

#[async_trait]
impl Tool for FileWriterTool {
    fn name(&self) -> &str {
        "file_writer"
    }

    fn description(&self) -> &str {
        "Write content to a file, creating it if it does not exist. Path must be within the workspace directory."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write (within workspace directory)"
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

        let target = resolve_workspace_path(path_str, &self.workspace_dir)?;

        // Ensure parent directory exists
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        tokio::fs::write(&target, content)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write file '{}': {}", path_str, e))?;

        Ok(format!(
            "Successfully wrote {} bytes to {}",
            content.len(),
            path_str
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_write_and_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = FileWriterTool::new(tmp.path().to_path_buf());
        let result = writer
            .execute(serde_json::json!({
                "path": "test.txt",
                "content": "hello world"
            }))
            .await
            .unwrap();
        assert!(result.contains("11 bytes"));

        let content = tokio::fs::read_to_string(tmp.path().join("test.txt"))
            .await
            .unwrap();
        assert_eq!(content, "hello world");
    }
}
