use std::fs;
use std::path::Path;

use slimbot::{
    truncate_text_head_tail, write_file_atomic, build_persisted_reference,
    TOOL_RESULTS_DIR, TOOL_RESULT_PREVIEW_CHARS,
};

// ── Text truncation ──

#[test]
fn test_truncate_short_text_unchanged() {
    assert_eq!(truncate_text_head_tail("hello", 100), "hello");
    assert_eq!(truncate_text_head_tail("", 10), "");
}

#[test]
fn test_truncate_exact_length_unchanged() {
    let text = "hello world";
    let result = truncate_text_head_tail(text, text.chars().count());
    assert_eq!(result, text);
}

#[test]
fn test_truncate_long_text_keeps_head_and_tail() {
    let long = "A".repeat(10_000);
    let truncated = truncate_text_head_tail(&long, 8000);

    assert!(truncated.contains("... (truncated,"));
    assert!(truncated.starts_with("A"));
    assert!(truncated.ends_with("A"));
    // The result includes the ellipsis line, so it may be slightly over max_chars
    // but the kept content (head + tail) equals max_chars.
    assert!(truncated.chars().count() < 10_000); // strictly less than original
}

#[test]
fn test_truncate_utf8_safe() {
    let cjk = "测试文本".repeat(2500); // 10000 chars
    let truncated = truncate_text_head_tail(&cjk, 8000);

    // Must be valid UTF-8
    assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    // Should start and end with CJK chars (not cut mid-char)
    assert!(truncated.starts_with("测试"));
    assert!(truncated.ends_with("文本"));
}

#[test]
fn test_truncate_emoji() {
    let emoji = "👋".repeat(5000); // 5000 emoji, each is 1 char
    let truncated = truncate_text_head_tail(&emoji, 4000);
    assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    assert!(truncated.starts_with("👋"));
    assert!(truncated.ends_with("👋"));
}

// ── Atomic file writes ──

#[test]
fn test_write_file_atomic_basic() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.txt");
    write_file_atomic(&path, "hello").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
}

#[test]
fn test_write_file_atomic_creates_parents() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("sub/dir/deep/file.txt");
    write_file_atomic(&path, "data").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "data");
}

#[test]
fn test_write_file_atomic_overwrites() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("existing.txt");
    fs::write(&path, "old content").unwrap();
    write_file_atomic(&path, "new content").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "new content");
}

#[test]
fn test_write_file_atomic_is_atomic() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("atomic.txt");

    // During write, there should be a .tmp file, and after completion only the target
    write_file_atomic(&path, "atomic data").unwrap();
    // No .tmp file should remain
    let entries: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    // Only our file should exist (in the sub dir, but temp should be clean)
    assert!(!entries.iter().any(|n| n.contains(".tmp")));
}

// ── Persisted references ──

#[test]
fn test_build_persisted_reference_short_content() {
    let content = "short result";
    let ref_str = build_persisted_reference(Path::new("/tmp/out.txt"), content, 100);
    // Short content doesn't need truncation notice
    assert!(ref_str.contains("[tool output persisted]"));
    assert!(ref_str.contains("out.txt"));
    assert!(ref_str.contains("Original size: 12"));
    assert!(!ref_str.contains("Read the saved file"));
}

#[test]
fn test_build_persisted_reference_long_content() {
    let long = "X".repeat(5000);
    let ref_str = build_persisted_reference(Path::new("/tmp/out.txt"), &long, 1200);
    assert!(ref_str.contains("[tool output persisted]"));
    assert!(ref_str.contains("Original size: 5000"));
    assert!(ref_str.contains("out.txt"));
    assert!(ref_str.contains("Read the saved file if you need the full output"));
}

#[test]
fn test_build_persisted_reference_preview_limit() {
    let content = "ABCDEFGHIJ";
    let ref_str = build_persisted_reference(Path::new("/tmp/x"), content, 5);
    assert!(ref_str.contains("ABCDE"));
    assert!(!ref_str.contains("FGHIJ")); // preview limited to 5
}

// ── Constants ──

#[test]
fn test_util_constants() {
    assert_eq!(TOOL_RESULTS_DIR, "tool-results");
    assert_eq!(TOOL_RESULT_PREVIEW_CHARS, 1200);
}
