use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use crate::tool::Tool;
use crate::tools::resolve_workspace_path;

/// Tool that performs search-and-replace in a file.
/// Requires old_string to appear exactly once in the file.
pub struct FileEditorTool {
    workspace_dir: PathBuf,
}

impl FileEditorTool {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir }
    }
}

#[async_trait]
impl Tool for FileEditorTool {
    fn name(&self) -> &str {
        "file_editor"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing an exact occurrence of old_string with new_string. old_string must appear exactly once in the file. Path must be within the workspace directory."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit (within workspace directory)"
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

        let resolved = resolve_workspace_path(path_str, &self.workspace_dir)?;

        if !resolved.is_file() {
            return Err(anyhow::anyhow!("Not a file: {}", path_str));
        }

        let content = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read file '{}': {}", path_str, e))?;

        let count = content.matches(old_string).count();
        if count == 0 {
            return Err(anyhow::anyhow!(
                "'old_string' not found in file '{}'",
                path_str
            ));
        }
        if count > 1 {
            return Err(anyhow::anyhow!(
                "'old_string' found {} times in '{}'. Provide a more specific string for unique match.",
                count,
                path_str
            ));
        }

        let new_content = content.replacen(old_string, new_string, 1);

        tokio::fs::write(&resolved, &new_content)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write file '{}': {}", path_str, e))?;

        Ok(format!(
            "Successfully edited {}: 1 replacement made",
            path_str
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_edit_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("edit.txt");
        tokio::fs::write(&file_path, "hello world").await.unwrap();

        let tool = FileEditorTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "path": "edit.txt",
                "old_string": "world",
                "new_string": "rust"
            }))
            .await
            .unwrap();
        assert!(result.contains("1 replacement"));

        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "hello rust");
    }

    #[tokio::test]
    async fn test_edit_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("edit.txt");
        tokio::fs::write(&file_path, "hello world").await.unwrap();

        let tool = FileEditorTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "path": "edit.txt",
                "old_string": "missing",
                "new_string": "rust"
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_edit_multiple_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("edit.txt");
        tokio::fs::write(&file_path, "foo foo").await.unwrap();

        let tool = FileEditorTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "path": "edit.txt",
                "old_string": "foo",
                "new_string": "bar"
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_edit_missing_path_arg() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = FileEditorTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({"old_string": "a", "new_string": "b"}))
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Missing required argument: path")
        );
    }

    #[tokio::test]
    async fn test_edit_missing_old_string_arg() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = FileEditorTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({"path": "edit.txt", "new_string": "b"}))
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Missing required argument: old_string")
        );
    }

    #[tokio::test]
    async fn test_edit_missing_new_string_arg() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("edit.txt"), "hello")
            .await
            .unwrap();
        let tool = FileEditorTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({"path": "edit.txt", "old_string": "hello"}))
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Missing required argument: new_string")
        );
    }

    #[tokio::test]
    async fn test_edit_not_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(tmp.path().join("adir"))
            .await
            .unwrap();
        let tool = FileEditorTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "path": "adir",
                "old_string": "a",
                "new_string": "b"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Not a file"));
    }

    #[tokio::test]
    async fn test_edit_rejects_path_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = FileEditorTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "path": "../../outside.txt",
                "old_string": "a",
                "new_string": "b"
            }))
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Path escapes workspace")
        );
    }

    #[tokio::test]
    async fn test_edit_empty_old_string_matches_everywhere() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("edit.txt"), "hello")
            .await
            .unwrap();
        let tool = FileEditorTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "path": "edit.txt",
                "old_string": "",
                "new_string": "x"
            }))
            .await;
        // Empty string matches at every position (len+1 times), so should error
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_edit_unicode_content() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("edit.txt"), "你好世界")
            .await
            .unwrap();
        let tool = FileEditorTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "path": "edit.txt",
                "old_string": "世界",
                "new_string": "Rust"
            }))
            .await
            .unwrap();
        assert!(result.contains("1 replacement"));
        let content = tokio::fs::read_to_string(tmp.path().join("edit.txt"))
            .await
            .unwrap();
        assert_eq!(content, "你好Rust");
    }
}
