use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use crate::tool::Tool;
use crate::tools::{protected, resolve_workspace_path};

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

    /// Risk class (ADR 0011): Writes files inside the workspace only.
    fn risk_level(&self) -> crate::approval::RiskLevel {
        crate::approval::RiskLevel::WorkspaceWrite
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

        // Protected documents are refused by code, not by prompt text (ADR
        // 0012). The refusal is a normal tool result so the run continues.
        if let Some(refusal) = protected::refusal_for_write(&target, &self.workspace_dir) {
            return Ok(refusal);
        }

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

    #[tokio::test]
    async fn test_write_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = FileWriterTool::new(tmp.path().to_path_buf());
        let result = writer
            .execute(serde_json::json!({
                "path": "sub/dir/test.txt",
                "content": "nested"
            }))
            .await
            .unwrap();
        assert!(result.contains("bytes"));
        assert!(result.contains("sub/dir/test.txt"));
        assert!(tmp.path().join("sub/dir/test.txt").exists());
    }

    #[tokio::test]
    async fn test_write_overwrites_existing() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("old.txt"), "old content")
            .await
            .unwrap();
        let writer = FileWriterTool::new(tmp.path().to_path_buf());
        let _ = writer
            .execute(serde_json::json!({
                "path": "old.txt",
                "content": "new content"
            }))
            .await
            .unwrap();
        let content = tokio::fs::read_to_string(tmp.path().join("old.txt"))
            .await
            .unwrap();
        assert_eq!(content, "new content");
    }

    #[tokio::test]
    async fn test_write_empty_content() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = FileWriterTool::new(tmp.path().to_path_buf());
        let result = writer
            .execute(serde_json::json!({
                "path": "empty.txt",
                "content": ""
            }))
            .await
            .unwrap();
        assert!(result.contains("0 bytes"));
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("empty.txt"))
                .await
                .unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn test_write_missing_path_arg() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = FileWriterTool::new(tmp.path().to_path_buf());
        let result = writer.execute(serde_json::json!({"content": "test"})).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Missing required argument: path")
        );
    }

    #[tokio::test]
    async fn test_write_missing_content_arg() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = FileWriterTool::new(tmp.path().to_path_buf());
        let result = writer
            .execute(serde_json::json!({"path": "test.txt"}))
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Missing required argument: content")
        );
    }

    #[tokio::test]
    async fn test_write_protected_document_is_refused_and_file_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("AGENTS.md");
        tokio::fs::write(&target, "owner rules").await.unwrap();

        let writer = FileWriterTool::new(tmp.path().to_path_buf());
        let result = writer
            .execute(serde_json::json!({
                "path": "AGENTS.md",
                "content": "agent rewrite"
            }))
            .await
            .expect("a refusal is a tool result, not a failed run");

        assert!(result.contains("REFUSED"), "result was: {result}");
        assert!(result.contains("AGENTS.md"), "must name the document");
        assert!(result.contains("owner-only"), "must state the rule");
        assert!(result.contains("Nothing was written"));
        assert_eq!(
            tokio::fs::read_to_string(&target).await.unwrap(),
            "owner rules"
        );
    }

    #[tokio::test]
    async fn test_write_every_bootstrap_document_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = FileWriterTool::new(tmp.path().to_path_buf());
        for doc in ["AGENTS.md", "SOUL.md", "USER.md", "TOOLS.md"] {
            let result = writer
                .execute(serde_json::json!({"path": doc, "content": "x"}))
                .await
                .unwrap();
            assert!(result.contains("REFUSED"), "{doc}: {result}");
            assert!(!tmp.path().join(doc).exists(), "{doc} must not be created");
        }
    }

    #[tokio::test]
    async fn test_write_protected_document_through_traversal_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(tmp.path().join("sub"))
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("SOUL.md"), "old soul")
            .await
            .unwrap();

        let writer = FileWriterTool::new(tmp.path().to_path_buf());
        let result = writer
            .execute(serde_json::json!({
                "path": "sub/../SOUL.md",
                "content": "new soul"
            }))
            .await
            .unwrap();
        assert!(result.contains("REFUSED"), "result was: {result}");
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("SOUL.md"))
                .await
                .unwrap(),
            "old soul"
        );
    }

    #[tokio::test]
    async fn test_write_memory_document_succeeds() {
        // The single-file funnel: MEMORY.md is the one document the reacting
        // model writes, and it must stay writable.
        let tmp = tempfile::tempdir().unwrap();
        let writer = FileWriterTool::new(tmp.path().to_path_buf());
        let result = writer
            .execute(serde_json::json!({
                "path": "memory/MEMORY.md",
                "content": "- fact (owner said · 2026-11-02)\n"
            }))
            .await
            .unwrap();
        assert!(!result.contains("REFUSED"), "result was: {result}");
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("memory/MEMORY.md"))
                .await
                .unwrap(),
            "- fact (owner said · 2026-11-02)\n"
        );
    }

    #[tokio::test]
    async fn test_write_runtime_files_stay_writable() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = FileWriterTool::new(tmp.path().to_path_buf());
        for path in ["cron/jobs.json", "HEARTBEAT.md", "memory/history.jsonl"] {
            let result = writer
                .execute(serde_json::json!({"path": path, "content": "{}"}))
                .await
                .unwrap();
            assert!(!result.contains("REFUSED"), "{path}: {result}");
        }
    }

    #[tokio::test]
    async fn test_write_rejects_path_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = FileWriterTool::new(tmp.path().to_path_buf());
        let result = writer
            .execute(serde_json::json!({
                "path": "../../outside.txt",
                "content": "escape attempt"
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
}
