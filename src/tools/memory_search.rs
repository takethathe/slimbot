//! `memory_search` — read-only keyword retrieval over memory entries.
//!
//! The main agent uses this to pull the few relevant paragraphs out of its
//! memory documents instead of reading whole files. The tool is strictly
//! read-only: it never writes memory, never persists an index and never
//! creates a file.

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::memory::category::MemoryCategory;
use crate::memory::{MAX_LIMIT, SharedMemory};
use crate::utils::truncate_text_head_tail;

/// Maximum characters of an entry's content echoed back per result.
const ENTRY_PREVIEW_CHARS: usize = 240;

/// `memory_search` — search memory entries by query.
pub struct MemorySearchTool {
    memory: SharedMemory,
}

impl MemorySearchTool {
    pub fn new(memory: SharedMemory) -> Self {
        Self { memory }
    }

    /// Parse the optional `limit`. Absent → backend default; non-integer or
    /// less than 1 → error; above the hard cap → clamped to [`MAX_LIMIT`].
    fn parse_limit(args: &Value) -> Result<Option<usize>> {
        match args.get("limit") {
            None | Some(Value::Null) => Ok(None),
            Some(value) => {
                let n = value.as_i64().ok_or_else(|| {
                    anyhow::anyhow!("invalid limit '{value}': expected a positive integer")
                })?;
                if n < 1 {
                    bail!("invalid limit {n}: expected a positive integer");
                }
                Ok(Some((n as usize).min(MAX_LIMIT)))
            }
        }
    }

    /// Parse the optional `category` filter. It narrows the result set; it is
    /// not a permission boundary (the main agent already holds `file_reader`).
    fn parse_category(args: &Value) -> Result<Option<MemoryCategory>> {
        match args.get("category") {
            None | Some(Value::Null) => Ok(None),
            Some(value) => {
                let raw = value.as_str().ok_or_else(|| {
                    anyhow::anyhow!("invalid category '{value}': expected a string")
                })?;
                MemoryCategory::from_str_opt(raw).map(Some).ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown category '{raw}'; allowed: soul, user, agent, tools, core, \
                         daily, skill, custom"
                    )
                })
            }
        }
    }

    fn format_results(query: &str, results: &[crate::memory::ScoredEntry]) -> String {
        let plural = if results.len() == 1 {
            "entry"
        } else {
            "entries"
        };
        let mut lines = vec![format!(
            "{} memory {plural} matched \"{query}\":",
            results.len()
        )];
        for scored in results {
            let content = truncate_text_head_tail(&scored.entry.content, ENTRY_PREVIEW_CHARS)
                .replace('\n', " ");
            let heading = match &scored.entry.heading {
                Some(heading) => format!("{heading} — "),
                None => String::new(),
            };
            lines.push(format!(
                "- [{}] {heading}{content}  ({})",
                scored.entry.category, scored.entry.source
            ));
        }
        lines.join("\n")
    }
}

#[async_trait]
impl crate::tool::Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search long-term memory documents for paragraphs relevant to a query \
         (Chinese and English are both supported). Returns the most relevant \
         entries with their category, enclosing heading and source document. \
         Read-only: it never modifies memory. Use it to recall a stored fact \
         instead of reading whole memory files."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords to search for. Chinese and English are both supported."
                },
                "category": {
                    "type": "string",
                    "enum": MemoryCategory::ALL
                        .iter()
                        .map(|c| c.as_str())
                        .collect::<Vec<_>>(),
                    "description": "Optional filter limiting the search to one memory category."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of entries to return (default 10, maximum 50)."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let query = match args.get("query").and_then(Value::as_str) {
            Some(query) => query.trim().to_string(),
            None => bail!("Missing required argument: query"),
        };
        if query.is_empty() {
            return Ok("No memory entries matched: the query is empty.".to_string());
        }

        let category = Self::parse_category(&args)?;
        let limit = Self::parse_limit(&args)?;
        let results = self
            .memory
            .lock()
            .await
            .search(&query, limit, category.as_ref())
            .await?;

        if results.is_empty() {
            return Ok(format!("No memory entries matched \"{query}\"."));
        }
        Ok(Self::format_results(&query, &results))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryService;
    use crate::tool::Tool;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    async fn make_tool() -> (MemorySearchTool, TempDir) {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let memory = Arc::new(Mutex::new(MemoryService::new(&workspace)));
        memory.lock().await.init().unwrap();
        (MemorySearchTool::new(memory), tmp)
    }

    async fn set(tool: &MemorySearchTool, category: &MemoryCategory, content: &str) {
        tool.memory
            .lock()
            .await
            .set(category, None, content)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_schema_requires_query_and_enumerates_categories() {
        let (tool, _tmp) = make_tool().await;
        let params = tool.parameters();
        assert_eq!(params["required"], json!(["query"]));
        assert_eq!(params["properties"]["query"]["type"], "string");
        assert_eq!(
            params["properties"]["category"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            8
        );
        assert_eq!(params["properties"]["limit"]["type"], "integer");
    }

    #[tokio::test]
    async fn test_execute_returns_category_source_and_heading() {
        let (tool, _tmp) = make_tool().await;
        set(
            &tool,
            &MemoryCategory::Core,
            "# Deployment\n\nRun the nightly deploy.\n",
        )
        .await;
        let out = tool.execute(json!({ "query": "deploy" })).await.unwrap();
        assert!(out.contains("[core]"), "output was: {out}");
        assert!(out.contains("Deployment"), "output was: {out}");
        assert!(out.contains("memory/MEMORY.md"), "output was: {out}");
    }

    #[tokio::test]
    async fn test_execute_no_match_returns_explicit_text() {
        let (tool, _tmp) = make_tool().await;
        set(&tool, &MemoryCategory::Core, "totally unrelated").await;
        let out = tool.execute(json!({ "query": "zzzz" })).await.unwrap();
        assert!(
            out.contains("No memory entries matched"),
            "output was: {out}"
        );
        assert!(!out.trim().is_empty());
    }

    #[tokio::test]
    async fn test_execute_empty_query_returns_explicit_text() {
        let (tool, _tmp) = make_tool().await;
        let out = tool.execute(json!({ "query": "   " })).await.unwrap();
        assert!(out.contains("the query is empty"), "output was: {out}");
    }

    #[tokio::test]
    async fn test_execute_missing_query_is_error() {
        let (tool, _tmp) = make_tool().await;
        assert!(tool.execute(json!({})).await.is_err());
    }

    #[tokio::test]
    async fn test_execute_category_filter_applies() {
        let (tool, _tmp) = make_tool().await;
        set(&tool, &MemoryCategory::Core, "shared keyword here").await;
        tool.memory
            .lock()
            .await
            .set(&MemoryCategory::User, None, "shared keyword here")
            .await
            .unwrap();
        let out = tool
            .execute(json!({ "query": "shared", "category": "user" }))
            .await
            .unwrap();
        assert!(out.contains("[user]"));
        assert!(!out.contains("[core]"));
    }

    #[tokio::test]
    async fn test_execute_unknown_category_is_error() {
        let (tool, _tmp) = make_tool().await;
        assert!(
            tool.execute(json!({ "query": "x", "category": "nope" }))
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({ "query": "x", "category": 3 }))
                .await
                .is_err()
        );
    }

    #[test]
    fn test_parse_limit_default_and_clamp() {
        assert_eq!(MemorySearchTool::parse_limit(&json!({})).unwrap(), None);
        assert_eq!(
            MemorySearchTool::parse_limit(&json!({ "limit": 3 })).unwrap(),
            Some(3)
        );
        assert_eq!(
            MemorySearchTool::parse_limit(&json!({ "limit": 999 })).unwrap(),
            Some(MAX_LIMIT)
        );
    }

    #[test]
    fn test_parse_limit_invalid_values_are_errors() {
        for bad in [
            json!({ "limit": 0 }),
            json!({ "limit": -2 }),
            json!({ "limit": "x" }),
            json!({ "limit": 1.5 }),
        ] {
            assert!(
                MemorySearchTool::parse_limit(&bad).is_err(),
                "limit {bad} must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn test_execute_limit_is_respected() {
        let (tool, _tmp) = make_tool().await;
        let body: String = (0..5).map(|i| format!("keyword {i}\n\n")).collect();
        set(&tool, &MemoryCategory::Core, &body).await;
        let out = tool
            .execute(json!({ "query": "keyword", "limit": 2 }))
            .await
            .unwrap();
        assert_eq!(out.matches("- [core]").count(), 2, "output was: {out}");
    }

    #[tokio::test]
    async fn test_execute_is_read_only() {
        let (tool, tmp) = make_tool().await;
        set(&tool, &MemoryCategory::Core, "some searchable content").await;
        let before = snapshot(tmp.path());
        let _ = tool
            .execute(json!({ "query": "searchable" }))
            .await
            .unwrap();
        let after = snapshot(tmp.path());
        assert_eq!(before, after, "memory_search must not write any file");
    }

    /// Sorted list of every file path under `root`, for read-only assertions.
    fn snapshot(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut paths = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path.clone());
                }
                paths.push(path);
            }
        }
        paths.sort();
        paths
    }
}
