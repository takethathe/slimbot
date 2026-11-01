use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use crate::tool::Tool;
use crate::tools::{protected, resolve_workspace_path};

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

    /// Risk class (ADR 0011): Edits files inside the workspace only (it can also read, but the write capability sets the class).
    fn risk_level(&self) -> crate::approval::RiskLevel {
        crate::approval::RiskLevel::WorkspaceWrite
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

        // Protected documents are refused by code, not by prompt text (ADR
        // 0012). The refusal is a normal tool result so the run continues.
        if let Some(refusal) = protected::refusal_for_write(&resolved, &self.workspace_dir) {
            return Ok(refusal);
        }

        if !resolved.is_file() {
            return Err(anyhow::anyhow!("Not a file: {}", path_str));
        }

        // The memory funnel (ADR 0014): when this edit lands on a memory
        // document, strip any echoed age marker from both sides *before*
        // matching (the file is stored clean, so an echoed marker in
        // `old_string` would otherwise fail to match), and keep a pre-write
        // snapshot for rollback. Both are no-ops for ordinary workspace files.
        let relative = crate::memory::snapshots::relative_document(&self.workspace_dir, &resolved);
        let managed = relative
            .as_deref()
            .is_some_and(crate::memory::snapshots::is_managed_document);
        let old_clean;
        let new_clean;
        let old_string: &str = if managed {
            old_clean = crate::memory::snapshots::strip_age_markers(old_string).0;
            &old_clean
        } else {
            old_string
        };
        let new_string: &str = if managed {
            new_clean = crate::memory::snapshots::strip_age_markers(new_string).0;
            &new_clean
        } else {
            new_string
        };

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

        if let Some(rel) = relative.as_deref() {
            crate::memory::snapshots::capture_before_write(&self.workspace_dir, rel)?;
        }

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
    async fn test_edit_protected_document_is_refused_and_file_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("USER.md"), "owner: Takethat")
            .await
            .unwrap();

        let tool = FileEditorTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "path": "USER.md",
                "old_string": "Takethat",
                "new_string": "nobody"
            }))
            .await
            .expect("a refusal is a tool result, not a failed run");

        assert!(result.contains("REFUSED"), "result was: {result}");
        assert!(result.contains("USER.md"));
        assert!(result.contains("owner-only"));
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("USER.md"))
                .await
                .unwrap(),
            "owner: Takethat"
        );
    }

    #[tokio::test]
    async fn test_edit_memory_document_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(tmp.path().join("memory"))
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("memory/MEMORY.md"), "- a\n")
            .await
            .unwrap();

        let tool = FileEditorTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "path": "memory/MEMORY.md",
                "old_string": "- a\n",
                "new_string": "- a\n- b (owner said · 2026-11-02)\n"
            }))
            .await
            .unwrap();
        assert!(result.contains("1 replacement"), "result was: {result}");
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("memory/MEMORY.md"))
                .await
                .unwrap(),
            "- a\n- b (owner said · 2026-11-02)\n"
        );
    }

    #[tokio::test]
    async fn test_edit_dream_owned_skill_document_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(tmp.path().join("skills/release"))
            .await
            .unwrap();
        let skill = tmp.path().join("skills/release/SKILL.md");
        tokio::fs::write(&skill, "# Release\n").await.unwrap();

        let tool = FileEditorTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "path": "skills/release/SKILL.md",
                "old_string": "# Release",
                "new_string": "# Pwned"
            }))
            .await
            .unwrap();
        assert!(result.contains("REFUSED"), "result was: {result}");
        assert_eq!(
            tokio::fs::read_to_string(&skill).await.unwrap(),
            "# Release\n"
        );
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

    #[tokio::test]
    async fn test_edit_memory_document_strips_echoed_marker_before_matching() {
        // The document is stored clean, so an age marker the model echoes into
        // `old_string` must be stripped *before* matching (ticket 06 #12), and
        // any marker in `new_string` is stripped before it lands.
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(tmp.path().join("memory"))
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("memory/MEMORY.md"), "- deploy via CI\n")
            .await
            .unwrap();
        let tool = FileEditorTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "path": "memory/MEMORY.md",
                "old_string": "- deploy via CI ← 3d",
                "new_string": "- deploy via CI (canary) ← 1d"
            }))
            .await
            .unwrap();
        assert!(result.contains("1 replacement"), "result was: {result}");
        let content = tokio::fs::read_to_string(tmp.path().join("memory/MEMORY.md"))
            .await
            .unwrap();
        assert_eq!(content, "- deploy via CI (canary)\n");

        // The pre-edit content was snapshotted for rollback.
        let store = crate::memory::snapshots::SnapshotStore::new(tmp.path());
        let snapshots = store.snapshots("memory/MEMORY.md").unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(
            store.read(&snapshots[0]).unwrap().as_deref(),
            Some("- deploy via CI\n")
        );
    }
}
