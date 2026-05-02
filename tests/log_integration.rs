use std::fs;
use std::process::Command;

/// Verify that --log-file creates a file with the expected format.
#[test]
fn test_log_file_created_with_format() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("test.log");
    let data_dir = tmp.path().join("data");

    let status = Command::new(env!("CARGO_BIN_EXE_slimbot"))
        .arg("--log=0")
        .arg(format!("--log-file={}", log_path.display()))
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("setup")
        .status()
        .expect("failed to execute binary");

    assert!(status.success(), "setup should exit successfully");

    let content = fs::read_to_string(&log_path).expect("log file should exist");
    assert!(!content.is_empty(), "log file should not be empty");

    // Verify format: lines like "[2026-05-02 14:30:00] [D] message"
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        assert!(
            line.starts_with("[20") && line.contains("] [") && line.contains("] "),
            "log line should match format [YYYY-MM-DD HH:MM:SS] [L] message: {line}",
        );
    }
}

/// Verify that --log=N suppresses entries below the threshold.
#[test]
fn test_log_level_filtering() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("test.log");
    let data_dir = tmp.path().join("data");

    // --log=2 (warning): should not contain [DEBUG] or [INFO]
    let status = Command::new(env!("CARGO_BIN_EXE_slimbot"))
        .arg("--log=2")
        .arg(format!("--log-file={}", log_path.display()))
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("setup")
        .status()
        .expect("failed to execute binary");

    assert!(status.success(), "setup should exit successfully");

    let content = fs::read_to_string(&log_path).expect("log file should exist");
    assert!(!content.contains("[D]"), "debug entries should be suppressed at level 2");
    assert!(!content.contains("[I]"), "info entries should be suppressed at level 2");
}
