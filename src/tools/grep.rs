use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;

use crate::tool::Tool;
use crate::tools::resolve_workspace_path;
use crate::utils::search::{
    is_binary, iter_files, matches_glob, matches_type, paginate, pagination_note,
};

const MAX_FILE_BYTES: usize = 2_000_000;
const MAX_RESULT_CHARS: usize = 128_000;
const DEFAULT_HEAD_LIMIT: usize = 250;

/// Tool that searches file contents using a regex (or plain text) pattern.
pub struct GrepTool {
    workspace_dir: PathBuf,
}

impl GrepTool {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir }
    }

    fn display_path(&self, abs: &Path, root: &Path) -> String {
        abs.strip_prefix(root)
            .unwrap_or(abs)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn format_block(
        display_path: &str,
        lines: &[String],
        match_line: usize,
        before: usize,
        after: usize,
    ) -> String {
        let start = match_line.saturating_sub(before).max(1);
        let end = (match_line + after).min(lines.len());
        let mut out = vec![format!("{}:{}", display_path, match_line)];
        for n in start..=end {
            let marker = if n == match_line { ">" } else { " " };
            out.push(format!("{} {:>4}| {}", marker, n, lines[n - 1]));
        }
        out.join("\n")
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regex pattern. Default output_mode is \
         files_with_matches (file paths only); use content mode for matching \
         lines with context. Skips binary files and files >2 MB. Supports \
         glob/type filtering and pagination via head_limit/offset."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex or plain text pattern to search for",
                    "minLength": 1
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in (default '.')"
                },
                "glob": {
                    "type": "string",
                    "description": "Optional file filter, e.g. '*.py' or 'tests/**/test_*.py'"
                },
                "type": {
                    "type": "string",
                    "description": "Optional file type shorthand, e.g. 'py', 'ts', 'md', 'json'"
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Case-insensitive search (default false)"
                },
                "fixed_strings": {
                    "type": "boolean",
                    "description": "Treat pattern as plain text instead of regex (default false)"
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "content: matching lines with optional context; files_with_matches: only matching file paths; count: matching line counts per file. Default: files_with_matches"
                },
                "context_before": {
                    "type": "integer",
                    "description": "Number of lines of context before each match",
                    "minimum": 0,
                    "maximum": 20
                },
                "context_after": {
                    "type": "integer",
                    "description": "Number of lines of context after each match",
                    "minimum": 0,
                    "maximum": 20
                },
                "head_limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return. Default 250, 0 for unlimited, max 1000",
                    "minimum": 0,
                    "maximum": 1000
                },
                "offset": {
                    "type": "integer",
                    "description": "Skip the first N results before applying head_limit",
                    "minimum": 0,
                    "maximum": 100000
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: pattern"))?;
        if pattern.is_empty() {
            return Err(anyhow::anyhow!("pattern must not be empty"));
        }

        let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let glob_filter = args.get("glob").and_then(|v| v.as_str());
        let type_filter = args.get("type").and_then(|v| v.as_str());
        let case_insensitive = args
            .get("case_insensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let fixed_strings = args
            .get("fixed_strings")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let output_mode = args
            .get("output_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("files_with_matches");
        let context_before = (args
            .get("context_before")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize)
            .min(20);
        let context_after = (args
            .get("context_after")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize)
            .min(20);
        let head_limit_raw = args.get("head_limit").and_then(|v| v.as_u64());
        let offset =
            (args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize).min(100_000);

        if !["content", "files_with_matches", "count"].contains(&output_mode) {
            return Err(anyhow::anyhow!(
                "output_mode must be one of: content, files_with_matches, count"
            ));
        }

        let limit: Option<usize> = match head_limit_raw {
            Some(0) => None,
            Some(n) => Some((n as usize).min(1000)),
            None => Some(DEFAULT_HEAD_LIMIT),
        };

        let workspace = self
            .workspace_dir
            .canonicalize()
            .unwrap_or(self.workspace_dir.clone());
        let resolved = resolve_workspace_path(path_str, &workspace)?;
        if !resolved.exists() {
            return Err(anyhow::anyhow!("Path not found: {}", path_str));
        }

        let regex_src = if fixed_strings {
            regex::escape(pattern)
        } else {
            pattern.to_string()
        };
        let re = if case_insensitive {
            Regex::new(&format!("(?i){}", regex_src))
        } else {
            Regex::new(&regex_src)
        }
        .map_err(|e| anyhow::anyhow!("Invalid regex pattern: {}", e))?;

        let root_for_walk = resolved.clone();
        let ws_for_display = workspace.clone();
        let files: Vec<PathBuf> = tokio::task::spawn_blocking(move || iter_files(&root_for_walk))
            .await
            .map_err(|e| anyhow::anyhow!("walk panicked: {}", e))?;

        let root = if resolved.is_dir() {
            resolved.clone()
        } else {
            resolved.parent().unwrap_or(&resolved).to_path_buf()
        };

        let mut matching_files: Vec<String> = Vec::new();
        let mut counts: Vec<(String, usize)> = Vec::new();
        let mut blocks: Vec<String> = Vec::new();
        let mut result_chars: usize = 0;
        let mut seen_content_matches: usize = 0;
        let mut truncated = false;
        let mut size_truncated = false;
        let mut skipped_binary = 0usize;
        let mut skipped_large = 0usize;
        let mut seen_files: HashSet<String> = HashSet::new();

        for file in &files {
            let rel_path = file
                .strip_prefix(&root)
                .unwrap_or(file)
                .to_string_lossy()
                .replace('\\', "/");
            let name = file.file_name().unwrap_or_default().to_string_lossy();

            if let Some(g) = glob_filter
                && !matches_glob(&rel_path, &name, g) {
                    continue;
                }
            if let Some(t) = type_filter
                && !matches_type(&name, t) {
                    continue;
                }

            let raw = match tokio::fs::read(file).await {
                Ok(r) => r,
                Err(_) => {
                    skipped_binary += 1;
                    continue;
                }
            };
            if raw.len() > MAX_FILE_BYTES {
                skipped_large += 1;
                continue;
            }
            if is_binary(&raw) {
                skipped_binary += 1;
                continue;
            }
            let content = match String::from_utf8(raw) {
                Ok(s) => s,
                Err(_) => {
                    skipped_binary += 1;
                    continue;
                }
            };

            let lines: Vec<String> = content.lines().map(String::from).collect();
            let display = self.display_path(file, &ws_for_display);
            let mut file_had_match = false;
            let mut file_match_count = 0usize;

            for (idx, line) in lines.iter().enumerate() {
                if !re.is_match(line) {
                    continue;
                }
                file_had_match = true;
                file_match_count += 1;
                let line_no = idx + 1;

                match output_mode {
                    "files_with_matches" => {
                        if seen_files.insert(display.clone()) {
                            matching_files.push(display.clone());
                        }
                        break;
                    }
                    "content" => {
                        seen_content_matches += 1;
                        if seen_content_matches <= offset {
                            continue;
                        }
                        if let Some(lim) = limit
                            && blocks.len() >= lim {
                                truncated = true;
                                break;
                            }
                        let block = Self::format_block(
                            &display,
                            &lines,
                            line_no,
                            context_before,
                            context_after,
                        );
                        let extra_sep = if blocks.is_empty() { 0 } else { 2 };
                        let block_len = block.len();
                        if result_chars + extra_sep + block_len > MAX_RESULT_CHARS {
                            size_truncated = true;
                            break;
                        }
                        blocks.push(block);
                        result_chars += extra_sep + block_len;
                    }
                    "count" => {}
                    _ => unreachable!(),
                }
            }

            if output_mode == "count" && file_had_match {
                counts.push((display, file_match_count));
            }
            if truncated || size_truncated {
                break;
            }
        }

        let mut notes: Vec<String> = Vec::new();
        let mut result: String = match output_mode {
            "files_with_matches" => {
                matching_files.sort();
                let (page, trunc) = paginate(&matching_files, limit, offset);
                truncated = truncated || trunc;
                if page.is_empty() {
                    format!("No matches found for pattern '{}' in {}", pattern, path_str)
                } else {
                    page.join("\n")
                }
            }
            "count" => {
                counts.sort_by(|a, b| a.0.cmp(&b.0));
                let names: Vec<String> = counts.iter().map(|(n, _)| n.clone()).collect();
                let (page, trunc) = paginate(&names, limit, offset);
                truncated = truncated || trunc;
                if page.is_empty() {
                    format!("No matches found for pattern '{}' in {}", pattern, path_str)
                } else {
                    let page_set: HashSet<&String> = page.iter().collect();
                    let lines: Vec<String> = counts
                        .iter()
                        .filter(|(n, _)| page_set.contains(n))
                        .map(|(n, c)| format!("{}: {}", n, c))
                        .collect();
                    let total: usize = counts.iter().map(|(_, c)| c).sum();
                    notes.push(format!(
                        "(total matches: {} in {} files)",
                        total,
                        counts.len()
                    ));
                    lines.join("\n")
                }
            }
            "content" => {
                if blocks.is_empty() {
                    format!("No matches found for pattern '{}' in {}", pattern, path_str)
                } else {
                    blocks.join("\n\n")
                }
            }
            _ => unreachable!(),
        };

        if output_mode == "content" {
            if truncated {
                notes.push(format!(
                    "(pagination: limit={}, offset={})",
                    limit.unwrap_or(0),
                    offset
                ));
            } else if size_truncated {
                notes.push("(output truncated due to size)".to_string());
            } else if let Some(n) = pagination_note(limit, offset, false) {
                notes.push(n);
            }
        } else if let Some(n) = pagination_note(limit, offset, truncated) {
            notes.push(n);
        }
        if skipped_binary > 0 {
            notes.push(format!(
                "(skipped {} binary/unreadable files)",
                skipped_binary
            ));
        }
        if skipped_large > 0 {
            notes.push(format!("(skipped {} large files)", skipped_large));
        }
        if !notes.is_empty() {
            result.push('\n');
            result.push_str(&notes.join("\n"));
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup() -> (tempfile::TempDir, GrepTool) {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("a.py"),
            "def foo():\n    return 1\n\ndef bar():\n    return 2\n",
        )
        .unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(
            tmp.path().join("sub/b.rs"),
            "fn main() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();
        fs::create_dir(tmp.path().join("target")).unwrap();
        fs::write(tmp.path().join("target/skip.py"), "foo bar").unwrap();
        fs::write(tmp.path().join("bin.dat"), b"\x00\x01\x02foo").unwrap();
        let tool = GrepTool::new(tmp.path().to_path_buf());
        (tmp, tool)
    }

    #[tokio::test]
    async fn test_grep_files_with_matches_default() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({"pattern": "foo", "path": "."}))
            .await
            .unwrap();
        assert!(r.contains("a.py"));
        assert!(!r.contains("target/"));
    }

    #[tokio::test]
    async fn test_grep_content_mode_with_context() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({
                "pattern": "return 2",
                "path": ".",
                "output_mode": "content",
                "context_before": 1,
                "context_after": 0
            }))
            .await
            .unwrap();
        assert!(r.contains(">"));
        assert!(r.contains("return 2"));
        assert!(r.contains("def bar"));
    }

    #[tokio::test]
    async fn test_grep_count_mode() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({
                "pattern": "return",
                "path": ".",
                "output_mode": "count"
            }))
            .await
            .unwrap();
        assert!(r.contains("a.py: 2"));
        assert!(r.contains("(total matches:"));
    }

    #[tokio::test]
    async fn test_grep_case_insensitive() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({"pattern": "DEF", "path": ".", "case_insensitive": true}))
            .await
            .unwrap();
        assert!(r.contains("a.py"));
    }

    #[tokio::test]
    async fn test_grep_fixed_strings() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({
                "pattern": "foo()",
                "path": ".",
                "fixed_strings": true
            }))
            .await
            .unwrap();
        assert!(r.contains("a.py"));
    }

    #[tokio::test]
    async fn test_grep_glob_filter() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({"pattern": "main", "path": ".", "glob": "*.rs"}))
            .await
            .unwrap();
        assert!(r.contains("b.rs"));
        assert!(!r.contains("a.py"));
    }

    #[tokio::test]
    async fn test_grep_type_filter() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({"pattern": "main", "path": ".", "type": "rust"}))
            .await
            .unwrap();
        assert!(r.contains("b.rs"));
    }

    #[tokio::test]
    async fn test_grep_pagination() {
        let (tmp, tool) = setup();
        for i in 0..5 {
            fs::write(tmp.path().join(format!("f{}.py", i)), "foo\nfoo\nfoo\n").unwrap();
        }
        let r = tool
            .execute(serde_json::json!({
                "pattern": "foo",
                "path": ".",
                "head_limit": 2,
                "offset": 0
            }))
            .await
            .unwrap();
        let path_lines: Vec<&str> = r.lines().take_while(|l| !l.starts_with('(')).collect();
        assert!(path_lines.len() <= 2);
        assert!(r.contains("pagination"));
    }

    #[tokio::test]
    async fn test_grep_no_matches() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({"pattern": "zzz_no_such", "path": "."}))
            .await
            .unwrap();
        assert!(r.contains("No matches found"));
    }

    #[tokio::test]
    async fn test_grep_path_not_found() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({"pattern": "x", "path": "does_not_exist"}))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn test_grep_invalid_output_mode() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({
                "pattern": "x",
                "path": ".",
                "output_mode": "bogus"
            }))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn test_grep_invalid_regex() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({"pattern": "[unclosed", "path": "."}))
            .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("Invalid regex"));
    }

    #[tokio::test]
    async fn test_grep_skips_binary_with_note() {
        let (tmp, tool) = setup();
        fs::write(tmp.path().join("plain.txt"), "foo").unwrap();
        let r = tool
            .execute(serde_json::json!({"pattern": "foo", "path": "."}))
            .await
            .unwrap();
        assert!(r.contains("skipped") && r.contains("binary"));
    }

    #[tokio::test]
    async fn test_grep_path_escape_rejected() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({"pattern": "x", "path": "../../etc/passwd"}))
            .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("escapes"));
    }
}
