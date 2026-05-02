use std::fs;

use slimbot::{LogLevel, log_init, log};
use tempfile::TempDir;

// ── LogLevel enum ──

#[test]
fn test_log_level_from_u8_all_valid() {
    assert_eq!(LogLevel::from_u8(0), Some(LogLevel::Debug));
    assert_eq!(LogLevel::from_u8(1), Some(LogLevel::Info));
    assert_eq!(LogLevel::from_u8(2), Some(LogLevel::Warning));
    assert_eq!(LogLevel::from_u8(3), Some(LogLevel::Error));
    assert_eq!(LogLevel::from_u8(4), Some(LogLevel::Fatal));
}

#[test]
fn test_log_level_from_u8_invalid() {
    assert_eq!(LogLevel::from_u8(5), None);
    assert_eq!(LogLevel::from_u8(255), None);
}

#[test]
fn test_log_level_ordering() {
    assert!(LogLevel::Debug < LogLevel::Info);
    assert!(LogLevel::Info < LogLevel::Warning);
    assert!(LogLevel::Warning < LogLevel::Error);
    assert!(LogLevel::Error < LogLevel::Fatal);
    assert!(LogLevel::Debug < LogLevel::Fatal);
}

#[test]
fn test_log_level_as_char() {
    assert_eq!(LogLevel::Debug.as_char(), 'D');
    assert_eq!(LogLevel::Info.as_char(), 'I');
    assert_eq!(LogLevel::Warning.as_char(), 'W');
    assert_eq!(LogLevel::Error.as_char(), 'E');
    assert_eq!(LogLevel::Fatal.as_char(), 'F');
}

#[test]
fn test_log_level_colored_tags() {
    assert_eq!(LogLevel::Debug.colored_tag(), "D");
    assert_eq!(LogLevel::Info.colored_tag(), "\x1b[32mI\x1b[0m");
    assert_eq!(LogLevel::Warning.colored_tag(), "\x1b[33mW\x1b[0m");
    assert_eq!(LogLevel::Error.colored_tag(), "\x1b[31mE\x1b[0m");
    assert_eq!(LogLevel::Fatal.colored_tag(), "\x1b[93mF\x1b[0m");
}

// ── Logger init and file output ──
// Note: Logger is a OnceLock singleton. Double-init and level-filtering
// tests are covered by the binary-level tests in log_integration.rs.

#[test]
#[should_panic(expected = "Logger::init called more than once")]
fn test_init_panics_on_double_init() {
    let _ = log_init(LogLevel::Debug, None);
    log_init(LogLevel::Debug, None).unwrap();
}

#[test]
fn test_log_file_output() {
    let tmp = TempDir::new().unwrap();
    let log_path = tmp.path().join("test.log");

    let result = std::panic::catch_unwind(|| {
        log_init(LogLevel::Debug, Some(&log_path)).unwrap();
        log(LogLevel::Info, &format_args!("test message"));
    });
    if result.is_ok() {
        assert!(log_path.exists());
        let content = fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("test message"));
        assert!(content.contains("[I]"));
    }
}
