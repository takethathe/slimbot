//! Shared utilities for search tools (grep, find_files).
//!
//! These are global helpers not tied to any specific tool instance.

use std::fs;
use std::path::{Path, PathBuf};

/// Directories skipped when walking the workspace.
pub(crate) const IGNORE_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    "dist",
    "build",
    ".cache",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".slimbot",
    ".claude",
];

/// Mapping from short language aliases to file-extension globs.
pub(crate) const TYPE_GLOB_MAP: &[(&str, &[&str])] = &[
    ("py", &["*.py", "*.pyi"]),
    ("python", &["*.py", "*.pyi"]),
    ("js", &["*.js", "*.jsx", "*.mjs", "*.cjs"]),
    ("ts", &["*.ts", "*.tsx", "*.mts", "*.cts"]),
    ("tsx", &["*.tsx"]),
    ("jsx", &["*.jsx"]),
    ("json", &["*.json"]),
    ("md", &["*.md", "*.mdx"]),
    ("markdown", &["*.md", "*.mdx"]),
    ("go", &["*.go"]),
    ("rs", &["*.rs"]),
    ("rust", &["*.rs"]),
    ("java", &["*.java"]),
    ("sh", &["*.sh", "*.bash"]),
    ("yaml", &["*.yaml", "*.yml"]),
    ("yml", &["*.yaml", "*.yml"]),
    ("toml", &["*.toml"]),
    ("sql", &["*.sql"]),
    ("html", &["*.html", "*.htm"]),
    ("css", &["*.css", "*.scss", "*.sass"]),
];

/// Recursively walk `root`, returning sorted file paths.
/// Directories in `IGNORE_DIRS` are skipped. `root` may be a single file
/// (returned as-is) or a directory.
pub(crate) fn iter_files(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            if IGNORE_DIRS.iter().any(|&ig| ig == name_str.as_ref()) {
                continue;
            }
            walk(&path, out);
        } else if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            out.push(path);
        }
    }
}

/// Detect binary content by inspecting raw bytes.
/// Returns true if the buffer contains a NUL byte or if the first 4096 bytes
/// have a high ratio of non-printable / non-whitespace characters (> 0.2).
pub(crate) fn is_binary(raw: &[u8]) -> bool {
    if raw.contains(&0) {
        return true;
    }
    let sample = &raw[..raw.len().min(4096)];
    if sample.is_empty() {
        return false;
    }
    let non_text = sample
        .iter()
        .filter(|&&b| b < 9 || (b > 13 && b < 32))
        .count();
    (non_text as f64 / sample.len() as f64) > 0.2
}

/// Match a file against a glob pattern.
/// If the pattern contains `/` or starts with `**`, match against the full
/// `rel_path`; otherwise match against the file name only.
pub(crate) fn matches_glob(rel_path: &str, name: &str, pattern: &str) -> bool {
    let normalized = pattern.trim().replace('\\', "/");
    if normalized.is_empty() {
        return false;
    }
    let Ok(pat) = glob::Pattern::new(&normalized) else {
        return false;
    };
    if normalized.contains('/') || normalized.starts_with("**") {
        pat.matches_path(&PathBuf::from(rel_path))
    } else {
        pat.matches(name)
    }
}

/// Check whether `name` matches a language/type alias.
/// Unknown aliases fall back to `*.<alias>`.
pub(crate) fn matches_type(name: &str, type_str: &str) -> bool {
    let lowered = type_str.trim().to_ascii_lowercase();
    if lowered.is_empty() {
        return true;
    }
    let patterns: Vec<&str> = TYPE_GLOB_MAP
        .iter()
        .find(|(alias, _)| *alias == lowered)
        .map(|(_, pats)| pats.to_vec())
        .unwrap_or_else(|| vec![""]);

    // Build fallback pattern like "*.xyz" for unknown aliases.
    let fallback;
    let effective_patterns: &[&str] = if patterns.len() == 1 && patterns[0].is_empty() {
        fallback = format!("*.{}", lowered);
        &[fallback.as_str()]
    } else {
        &patterns
    };

    let lower_name = name.to_ascii_lowercase();
    effective_patterns.iter().any(|p| {
        glob::Pattern::new(p)
            .map(|pat| pat.matches(&lower_name))
            .unwrap_or(false)
    })
}

/// Slice `items` by `(offset, offset + limit)`. Returns the slice and a
/// `truncated` flag indicating whether more items existed past the slice.
pub(crate) fn paginate<T>(items: &[T], limit: Option<usize>, offset: usize) -> (&[T], bool) {
    if offset >= items.len() {
        return (&[], false);
    }
    let rest = &items[offset..];
    match limit {
        None => (rest, false),
        Some(lim) => {
            let end = rest.len().min(lim);
            (&rest[..end], rest.len() > end)
        }
    }
}

/// Render a pagination note (e.g. `(pagination: limit=10, offset=5)`).
/// Returns `None` if no pagination info needs surfacing.
pub(crate) fn pagination_note(
    limit: Option<usize>,
    offset: usize,
    truncated: bool,
) -> Option<String> {
    if truncated {
        return Some(match limit {
            Some(l) => format!("(pagination: limit={}, offset={})", l, offset),
            None => format!("(pagination: offset={})", offset),
        });
    }
    if offset > 0 {
        return Some(format!("(pagination: offset={})", offset));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_is_binary_with_null() {
        assert!(is_binary(b"hello\x00world"));
    }

    #[test]
    fn test_is_binary_plain_text() {
        assert!(!is_binary(b"hello world\nsecond line"));
    }

    #[test]
    fn test_is_binary_empty() {
        assert!(!is_binary(b""));
    }

    #[test]
    fn test_is_binary_high_non_text_ratio() {
        let data: Vec<u8> = (0..100)
            .map(|i| if i % 5 == 0 { b'a' } else { 1 })
            .collect();
        assert!(is_binary(&data));
    }

    #[test]
    fn test_matches_glob_name_only() {
        assert!(matches_glob("foo/bar.py", "bar.py", "*.py"));
        assert!(!matches_glob("foo/bar.py", "bar.py", "*.rs"));
    }

    #[test]
    fn test_matches_glob_rel_path() {
        assert!(matches_glob(
            "tests/foo_test.rs",
            "foo_test.rs",
            "tests/**/*.rs"
        ));
        assert!(!matches_glob("src/foo.rs", "foo.rs", "tests/**/*.rs"));
    }

    #[test]
    fn test_matches_glob_empty() {
        assert!(!matches_glob("a.py", "a.py", ""));
    }

    #[test]
    fn test_matches_type_known_alias() {
        assert!(matches_type("foo.py", "py"));
        assert!(matches_type("foo.py", "python"));
        assert!(matches_type("FOO.PY", "py"));
        assert!(!matches_type("foo.rs", "py"));
    }

    #[test]
    fn test_matches_type_unknown_alias_fallback() {
        assert!(matches_type("foo.xyz", "xyz"));
        assert!(!matches_type("foo.py", "xyz"));
    }

    #[test]
    fn test_matches_type_empty() {
        assert!(matches_type("anything", ""));
    }

    #[test]
    fn test_iter_files_sorts_and_skips_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "").unwrap();
        fs::write(tmp.path().join("b.txt"), "").unwrap();
        fs::create_dir(tmp.path().join("target")).unwrap();
        fs::write(tmp.path().join("target/skip.txt"), "").unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/c.txt"), "").unwrap();

        let files = iter_files(tmp.path());
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn test_iter_files_single_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("x.txt");
        fs::write(&file, "hi").unwrap();
        let files = iter_files(&file);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], file);
    }

    #[test]
    fn test_paginate_no_limit() {
        let items = vec![1, 2, 3, 4, 5];
        let (slice, truncated) = paginate(&items, None, 0);
        assert_eq!(slice, &[1, 2, 3, 4, 5]);
        assert!(!truncated);
    }

    #[test]
    fn test_paginate_with_limit() {
        let items = vec![1, 2, 3, 4, 5];
        let (slice, truncated) = paginate(&items, Some(2), 0);
        assert_eq!(slice, &[1, 2]);
        assert!(truncated);
    }

    #[test]
    fn test_paginate_with_offset() {
        let items = vec![1, 2, 3, 4, 5];
        let (slice, truncated) = paginate(&items, Some(2), 2);
        assert_eq!(slice, &[3, 4]);
        assert!(truncated);
    }

    #[test]
    fn test_paginate_offset_past_end() {
        let items = vec![1, 2, 3];
        let (slice, truncated) = paginate(&items, Some(10), 100);
        assert!(slice.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn test_pagination_note_truncated_with_limit() {
        let note = pagination_note(Some(10), 5, true).unwrap();
        assert_eq!(note, "(pagination: limit=10, offset=5)");
    }

    #[test]
    fn test_pagination_note_truncated_no_limit() {
        let note = pagination_note(None, 5, true).unwrap();
        assert_eq!(note, "(pagination: offset=5)");
    }

    #[test]
    fn test_pagination_note_offset_only() {
        let note = pagination_note(None, 5, false).unwrap();
        assert_eq!(note, "(pagination: offset=5)");
    }

    #[test]
    fn test_pagination_note_none() {
        assert!(pagination_note(None, 0, false).is_none());
        assert!(pagination_note(Some(10), 0, false).is_none());
    }
}
