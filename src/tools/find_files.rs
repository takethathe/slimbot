use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;

use crate::tool::Tool;
use crate::tools::resolve_workspace_path;
use crate::utils::search::{
    IGNORE_DIRS, iter_files, matches_glob, matches_type, paginate, pagination_note,
};

const DEFAULT_FILE_HEAD_LIMIT: usize = 200;

/// Tool that finds files by path fragment, glob, or file type.
pub struct FindFilesTool {
    workspace_dir: PathBuf,
}

impl FindFilesTool {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir }
    }

    fn display_path(&self, abs: &Path, root: &Path) -> String {
        abs.strip_prefix(root)
            .unwrap_or(abs)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

#[async_trait]
impl Tool for FindFilesTool {
    fn name(&self) -> &str {
        "find_files"
    }

    fn description(&self) -> &str {
        "Find files by path fragment, glob, or file type. Use this before \
         file_reader when you need to locate files. Returns workspace-relative \
         paths and skips common dependency/build directories."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory or file to search in (default '.')"
                },
                "query": {
                    "type": "string",
                    "description": "Optional case-insensitive path fragment search. Whitespace-separated terms must all be present."
                },
                "glob": {
                    "type": "string",
                    "description": "Optional file filter, e.g. '*.py' or 'tests/**/test_*.py'"
                },
                "type": {
                    "type": "string",
                    "description": "Optional file type shorthand, e.g. 'py', 'ts', 'md', 'json'"
                },
                "include_dirs": {
                    "type": "boolean",
                    "description": "Include matching directories as well as files (default false)"
                },
                "sort": {
                    "type": "string",
                    "enum": ["path", "modified"],
                    "description": "Sort by path or most recently modified first (default path)"
                },
                "head_limit": {
                    "type": "integer",
                    "description": "Maximum number of paths to return (default 200, 0 for all, max 1000)",
                    "minimum": 0,
                    "maximum": 1000
                },
                "offset": {
                    "type": "integer",
                    "description": "Skip the first N results before applying head_limit",
                    "minimum": 0,
                    "maximum": 100000
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let query = args.get("query").and_then(|v| v.as_str());
        let glob_filter = args.get("glob").and_then(|v| v.as_str());
        let type_filter = args.get("type").and_then(|v| v.as_str());
        let include_dirs = args
            .get("include_dirs")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let sort = args.get("sort").and_then(|v| v.as_str()).unwrap_or("path");
        let head_limit_raw = args.get("head_limit").and_then(|v| v.as_u64());
        let offset =
            (args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize).min(100_000);

        if !["path", "modified"].contains(&sort) {
            return Err(anyhow::anyhow!("sort must be 'path' or 'modified'"));
        }

        let limit: Option<usize> = match head_limit_raw {
            Some(0) => None,
            Some(n) => Some((n as usize).min(1000)),
            None => Some(DEFAULT_FILE_HEAD_LIMIT),
        };

        let workspace = self
            .workspace_dir
            .canonicalize()
            .unwrap_or(self.workspace_dir.clone());
        let resolved = resolve_workspace_path(path_str, &workspace)?;
        if !resolved.exists() {
            return Err(anyhow::anyhow!("Path not found: {}", path_str));
        }

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

        let query_terms: Option<Vec<String>> = query.map(|q| {
            q.split_whitespace()
                .map(|s| s.to_ascii_lowercase())
                .collect()
        });

        let mut matches: Vec<(String, f64)> = Vec::new();

        if include_dirs {
            let resolved_for_dirs = resolved.clone();
            let mut dirs: Vec<PathBuf> = tokio::task::spawn_blocking(move || {
                let mut dirs = Vec::new();
                collect_dirs(&resolved_for_dirs, &mut dirs);
                dirs
            })
            .await
            .map_err(|e| anyhow::anyhow!("dir walk panicked: {}", e))?;
            dirs.sort();
            for dir in dirs {
                let rel = dir
                    .strip_prefix(&root)
                    .unwrap_or(&dir)
                    .to_string_lossy()
                    .replace('\\', "/");
                let display = self.display_path(&dir, &ws_for_display);
                let name = dir.file_name().unwrap_or_default().to_string_lossy();

                if let Some(g) = glob_filter
                    && !matches_glob(&rel, &name, g) {
                        continue;
                    }
                if type_filter.is_some() {
                    continue;
                }
                if let Some(terms) = &query_terms {
                    let hay = display.to_ascii_lowercase();
                    if !terms.iter().all(|t| hay.contains(t)) {
                        continue;
                    }
                }
                let mtime = tokio::fs::metadata(&dir)
                    .await
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                matches.push((format!("{}/", display), mtime));
            }
        }

        for file in &files {
            if !file.is_file() {
                continue;
            }
            let rel = file
                .strip_prefix(&root)
                .unwrap_or(file)
                .to_string_lossy()
                .replace('\\', "/");
            let name = file.file_name().unwrap_or_default().to_string_lossy();
            let display = self.display_path(file, &ws_for_display);

            if let Some(g) = glob_filter
                && !matches_glob(&rel, &name, g) {
                    continue;
                }
            if let Some(t) = type_filter
                && !matches_type(&name, t) {
                    continue;
                }
            if let Some(terms) = &query_terms {
                let hay = display.to_ascii_lowercase();
                if !terms.iter().all(|t| hay.contains(t)) {
                    continue;
                }
            }

            let mtime = tokio::fs::metadata(file)
                .await
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            matches.push((display, mtime));
        }

        match sort {
            "modified" => matches.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.0.cmp(&b.0))
            }),
            _ => matches.sort_by(|a, b| a.0.cmp(&b.0)),
        }

        let paths: Vec<String> = matches.into_iter().map(|(p, _)| p).collect();
        let (page, truncated) = paginate(&paths, limit, offset);

        if page.is_empty() {
            return Ok("No files found".to_string());
        }

        let mut result = page.join("\n");
        if let Some(note) = pagination_note(limit, offset, truncated) {
            result.push_str("\n\n");
            result.push_str(&note);
        }
        Ok(result)
    }
}

fn collect_dirs(root: &Path, out: &mut Vec<PathBuf>) {
    if !root.is_dir() {
        return;
    }
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !path.is_dir() {
            continue;
        }
        if IGNORE_DIRS.iter().any(|&ig| ig == name_str.as_ref()) {
            continue;
        }
        out.push(path.clone());
        collect_dirs(&path, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup() -> (tempfile::TempDir, FindFilesTool) {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.py"), "").unwrap();
        fs::write(tmp.path().join("b.rs"), "").unwrap();
        fs::write(tmp.path().join("readme.md"), "").unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/c.py"), "").unwrap();
        fs::create_dir(tmp.path().join("target")).unwrap();
        fs::write(tmp.path().join("target/skip.py"), "").unwrap();
        let tool = FindFilesTool::new(tmp.path().to_path_buf());
        (tmp, tool)
    }

    #[tokio::test]
    async fn test_find_files_basic() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({"path": "."}))
            .await
            .unwrap();
        assert!(r.contains("a.py"));
        assert!(r.contains("b.rs"));
        assert!(!r.contains("target/"));
    }

    #[tokio::test]
    async fn test_find_files_query() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({"path": ".", "query": "readme"}))
            .await
            .unwrap();
        assert!(r.contains("readme.md"));
        assert!(!r.contains("a.py"));
    }

    #[tokio::test]
    async fn test_find_files_glob() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({"path": ".", "glob": "*.py"}))
            .await
            .unwrap();
        assert!(r.contains("a.py"));
        assert!(r.contains("c.py"));
        assert!(!r.contains("b.rs"));
    }

    #[tokio::test]
    async fn test_find_files_type_filter() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({"path": ".", "type": "rust"}))
            .await
            .unwrap();
        assert!(r.contains("b.rs"));
        assert!(!r.contains("a.py"));
    }

    #[tokio::test]
    async fn test_find_files_include_dirs() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({"path": ".", "include_dirs": true}))
            .await
            .unwrap();
        assert!(r.contains("sub/"));
    }

    #[tokio::test]
    async fn test_find_files_sort_modified() {
        let (_tmp, tool) = setup();
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(tool.workspace_dir.join("newest.py"), "").unwrap();
        let r = tool
            .execute(serde_json::json!({"path": ".", "sort": "modified"}))
            .await
            .unwrap();
        let first_line = r.lines().next().unwrap_or("");
        assert!(first_line.contains("newest.py"));
    }

    #[tokio::test]
    async fn test_find_files_pagination() {
        let (tmp, tool) = setup();
        for i in 0..5 {
            fs::write(tmp.path().join(format!("x{}.py", i)), "").unwrap();
        }
        let r = tool
            .execute(serde_json::json!({
                "path": ".",
                "glob": "*.py",
                "head_limit": 2
            }))
            .await
            .unwrap();
        let path_lines: Vec<&str> = r.lines().take_while(|l| !l.is_empty()).collect();
        assert!(path_lines.len() <= 2);
        assert!(r.contains("pagination"));
    }

    #[tokio::test]
    async fn test_find_files_empty() {
        let empty = tempfile::tempdir().unwrap();
        let tool = FindFilesTool::new(empty.path().to_path_buf());
        let r = tool
            .execute(serde_json::json!({"path": "."}))
            .await
            .unwrap();
        assert_eq!(r, "No files found");
    }

    #[tokio::test]
    async fn test_find_files_path_not_found() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({"path": "no_such_dir"}))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn test_find_files_invalid_sort() {
        let (_tmp, tool) = setup();
        let r = tool
            .execute(serde_json::json!({"path": ".", "sort": "bogus"}))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn test_find_files_path_escape() {
        let (_tmp, tool) = setup();
        let r = tool.execute(serde_json::json!({"path": "../../etc"})).await;
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("escapes"));
    }
}
