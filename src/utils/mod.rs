use std::fs;
use std::path::Path;

/// Truncate text by keeping head + tail portions, with an ellipsis in between.
/// `max_chars` is a character count (not byte count).
pub fn truncate_text_head_tail(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    let tail_size = HEAD_TAIL_CHUNK.min(max_chars / 2);
    let head_size = max_chars - tail_size;
    let head_end = text.char_indices()
        .nth(head_size)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let tail_start = text.char_indices()
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
pub fn build_persisted_reference(
    file_path: &Path,
    content: &str,
    preview_max: usize,
) -> String {
    let preview = &content[..preview_max.min(content.len())];
    let truncated = content.len() > preview_max;
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

/// Default preview size for persisted tool results.
pub const TOOL_RESULT_PREVIEW_CHARS: usize = 1200;

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
}
