use std::fs;
use std::path::Path;

pub(crate) mod search;

/// Truncate text by keeping head + tail portions, with an ellipsis in between.
/// `max_chars` is a character count (not byte count).
pub fn truncate_text_head_tail(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    let tail_size = HEAD_TAIL_CHUNK.min(max_chars / 2);
    let head_size = max_chars - tail_size;
    let head_end = text
        .char_indices()
        .nth(head_size)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let tail_start = text
        .char_indices()
        .nth(char_count - tail_size)
        .map(|(i, _)| i)
        .unwrap_or(text.len());

    let omitted_count = char_count - head_size - tail_size;
    format!(
        "{}\n... (truncated, {} chars omitted) ...\n{}",
        &text[..head_end],
        omitted_count,
        &text[tail_start..],
    )
}

const HEAD_TAIL_CHUNK: usize = 2000;

/// Write content to a file atomically using a temp file + rename.
/// Creates parent directories if needed.
pub fn write_file_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default(),
    ));
    fs::write(&tmp_path, content)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Build a reference string for a persisted tool result.
/// Returns a stable format: reference + preview + truncation notice.
pub fn build_persisted_reference(file_path: &Path, content: &str, preview_max: usize) -> String {
    let preview_end = content
        .char_indices()
        .nth(preview_max)
        .map(|(i, _)| i)
        .unwrap_or(content.len());
    let preview = &content[..preview_end];
    let truncated = content.chars().count() > preview_max;
    let mut result = format!(
        "[tool output persisted]\nFull output saved to: {}\nOriginal size: {} chars\nPreview:\n{}",
        file_path.display(),
        content.len(),
        preview,
    );
    if truncated {
        result.push_str("\n...\n(Read the saved file if you need the full output.)");
    }
    result
}

/// Default directory name for persisted tool results.
pub const TOOL_RESULTS_DIR: &str = "tool-results";

/// Workspace-relative path used in context references for a persisted result.
///
/// Relative on purpose: the reference travels inside the session JSONL and must
/// stay meaningful even if the data directory moves.
pub fn tool_result_rel_path(tool_call_id: &str) -> String {
    format!("{}/{}.txt", TOOL_RESULTS_DIR, tool_call_id)
}

/// Path of an already-persisted tool result, if the file exists.
pub fn existing_tool_result_path(workspace_dir: &Path, tool_call_id: &str) -> Option<String> {
    let path = workspace_dir
        .join(TOOL_RESULTS_DIR)
        .join(format!("{}.txt", tool_call_id));
    path.is_file().then(|| tool_result_rel_path(tool_call_id))
}

/// Persist a tool result verbatim under `tool-results/<tool_call_id>.txt`.
///
/// Returns the workspace-relative path on success. Existing files are never
/// overwritten (the first, full copy wins); cleanup uses this to re-persist a
/// result that was trimmed without ever having been written to disk.
pub fn persist_tool_result_file(
    workspace_dir: &Path,
    tool_call_id: &str,
    content: &str,
) -> Option<String> {
    let path = workspace_dir
        .join(TOOL_RESULTS_DIR)
        .join(format!("{}.txt", tool_call_id));
    if !path.exists() {
        write_file_atomic(&path, content).ok()?;
    }
    Some(tool_result_rel_path(tool_call_id))
}

/// Delete persisted tool results older than `ttl`, returning how many went away.
///
/// `tool-results/` only grows when context pressure trims a result, and each
/// trimmed reference carries a path that should stay recoverable for a while.
/// The sweep rides Dream's success branch instead of starting a new timer.
pub fn sweep_tool_results(workspace_dir: &Path, ttl: std::time::Duration) -> usize {
    let results_dir = workspace_dir.join(TOOL_RESULTS_DIR);
    let Ok(entries) = fs::read_dir(&results_dir) else {
        return 0;
    };
    let mut deleted = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(age) = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|m| m.elapsed().map_err(std::io::Error::other))
        else {
            continue;
        };
        if age > ttl && fs::remove_file(&path).is_ok() {
            deleted += 1;
        }
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_short_text() {
        assert_eq!(truncate_text_head_tail("hello", 100), "hello");
    }

    #[test]
    fn test_truncate_long_text() {
        let long = "A".repeat(10_000);
        let truncated = truncate_text_head_tail(&long, 8000);
        assert!(truncated.contains("... (truncated,"));
        assert!(truncated.starts_with("A"));
        assert!(truncated.ends_with("A"));
    }

    #[test]
    fn test_truncate_utf8_boundary() {
        let cjk = "测试文本".repeat(2500); // 10000 chars
        let truncated = truncate_text_head_tail(&cjk, 8000);
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
        assert!(truncated.starts_with("测试"));
        assert!(truncated.ends_with("文本"));
    }

    #[test]
    fn test_write_file_atomic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.txt");
        write_file_atomic(&path, "hello").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn test_write_file_atomic_creates_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sub/dir/file.txt");
        write_file_atomic(&path, "data").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "data");
    }

    #[test]
    fn test_build_persisted_reference() {
        let long = "X".repeat(5000);
        let ref_str = build_persisted_reference(Path::new("/tmp/out.txt"), &long, 1200);
        assert!(ref_str.contains("[tool output persisted]"));
        assert!(ref_str.contains("Original size: 5000"));
        assert!(ref_str.contains("out.txt"));
    }

    #[test]
    fn test_build_persisted_reference_utf8() {
        let cjk = "测试文本".repeat(2000);
        let ref_str = build_persisted_reference(Path::new("/tmp/out.txt"), &cjk, 1200);
        assert!(std::str::from_utf8(ref_str.as_bytes()).is_ok());
    }

    /// Backdate a file's mtime by `days` so age-based tests do not sleep.
    fn backdate(path: &Path, days: u64) {
        use std::fs::FileTimes;
        let file = std::fs::File::options().write(true).open(path).unwrap();
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
        file.set_times(FileTimes::new().set_modified(when)).unwrap();
    }

    #[test]
    fn test_sweep_tool_results_removes_expired_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(TOOL_RESULTS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.txt");
        std::fs::write(&old, "full output").unwrap();
        backdate(&old, 8);

        let removed = sweep_tool_results(tmp.path(), std::time::Duration::from_secs(7 * 86_400));

        assert_eq!(removed, 1);
        assert!(
            !old.exists(),
            "an expired reference is no longer recoverable"
        );
    }

    #[test]
    fn test_sweep_tool_results_keeps_fresh_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(TOOL_RESULTS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let fresh = dir.join("fresh.txt");
        std::fs::write(&fresh, "full output").unwrap();
        backdate(&fresh, 1);

        let removed = sweep_tool_results(tmp.path(), std::time::Duration::from_secs(7 * 86_400));

        assert_eq!(removed, 0);
        assert!(fresh.exists(), "references inside the TTL stay recoverable");
    }

    #[test]
    fn test_sweep_tool_results_missing_directory_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let removed = sweep_tool_results(tmp.path(), std::time::Duration::from_secs(86_400));
        assert_eq!(removed, 0);
    }

    #[test]
    fn test_persist_tool_result_file_never_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let first = persist_tool_result_file(tmp.path(), "tc-1", "original").unwrap();
        let second = persist_tool_result_file(tmp.path(), "tc-1", "different").unwrap();

        assert_eq!(first, second);
        let on_disk = std::fs::read_to_string(tmp.path().join(&first)).unwrap();
        assert_eq!(on_disk, "original", "the first full copy wins");
    }
}
