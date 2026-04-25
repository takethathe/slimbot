use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use crate::tool::Tool;
use crate::tools::resolve_workspace_path;

/// Tool that lists directory contents.
pub struct ListDirTool {
    workspace_dir: PathBuf,
}

impl ListDirTool {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir }
    }
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List the contents of a directory. Returns entries prefixed with [D] for directories, [F] for files, [L] for links. Path must be within the workspace directory."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the directory to list (within workspace directory)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;

        let resolved = resolve_workspace_path(path_str, &self.workspace_dir)?;

        if !resolved.is_dir() {
            return Err(anyhow::anyhow!("Not a directory: {}", path_str));
        }

        let mut entries = tokio::fs::read_dir(&resolved).await?;
        let mut lines = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            let ft = entry.file_type().await?;
            let prefix = if ft.is_dir() {
                "[D] "
            } else if ft.is_file() {
                "[F] "
            } else {
                "[L] "
            };
            lines.push(format!("{}{}", prefix, name));
        }

        if lines.is_empty() {
            Ok("(empty directory)".to_string())
        } else {
            lines.sort();
            Ok(lines.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_list_dir() {
        let tmp = tempfile::tempdir().unwrap();
        // Create some files
        tokio::fs::write(tmp.path().join("a.txt"), "")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("b.txt"), "")
            .await
            .unwrap();

        let tool = ListDirTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({"path": "."}))
            .await
            .unwrap();
        assert!(result.contains("a.txt"));
        assert!(result.contains("b.txt"));
    }

    #[tokio::test]
    async fn test_list_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ListDirTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({"path": "."}))
            .await
            .unwrap();
        assert_eq!(result, "(empty directory)");
    }

    #[tokio::test]
    async fn test_list_not_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("file.txt"), "hello")
            .await
            .unwrap();

        let tool = ListDirTool::new(tmp.path().to_path_buf());
        let result = tool.execute(serde_json::json!({"path": "file.txt"})).await;
        assert!(result.is_err());
    }
}
