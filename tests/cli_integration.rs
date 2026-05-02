use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn slimbot_bin() -> std::process::Command {
    Command::new(env!("CARGO_BIN_EXE_slimbot"))
}

#[allow(dead_code)]
fn setup_test_config(data_dir: &std::path::Path) -> String {
    std::fs::create_dir_all(data_dir).unwrap();
    let config_path = data_dir.join("config.json");
    let config = serde_json::json!({
        "agent": { "provider": "default" },
        "providers": {
            "default": {
                "api_key": "sk-test-key-not-real",
                "model": "gpt-4o",
                "base_url": "https://api.openai.com"
            }
        }
    });
    fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    config_path.to_str().unwrap().to_string()
}

// ── CLI argument parsing ──

#[test]
fn test_help_exits_successfully() {
    let output = slimbot_bin().arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("slimbot"));
    assert!(stdout.contains("--config"));
    assert!(stdout.contains("--data-dir"));
    assert!(stdout.contains("--log"));
    assert!(stdout.contains("--log-file"));
    assert!(stdout.contains("setup"));
    assert!(stdout.contains("agent"));
}

#[test]
fn test_no_subcommand_shows_help() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let output = slimbot_bin()
        .arg(format!("--data-dir={}", data_dir.display()))
        .output()
        .unwrap();
    // No subcommand → shows help and exits
    assert!(output.status.success());
}

// ── Setup subcommand ──

#[test]
fn test_setup_creates_directories() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");

    let output = slimbot_bin()
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("setup")
        .output()
        .unwrap();

    assert!(output.status.success(), "setup should succeed");
    assert!(data_dir.exists());
    assert!(data_dir.join("workspace").exists());
}

#[test]
fn test_setup_creates_bootstrap_files() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");

    slimbot_bin()
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("setup")
        .output()
        .unwrap();

    let ws = data_dir.join("workspace");
    assert!(ws.join("AGENTS.md").exists());
    assert!(ws.join("USER.md").exists());
    assert!(ws.join("SOUL.md").exists());
    assert!(ws.join("TOOLS.md").exists());
}

#[test]
fn test_setup_creates_config() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let config_path = data_dir.join("config.json");

    slimbot_bin()
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("setup")
        .output()
        .unwrap();

    assert!(config_path.exists());
    let content = fs::read_to_string(&config_path).unwrap();
    assert!(content.contains("provider"));
}

#[test]
fn test_setup_idempotent() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");

    // Run setup twice
    slimbot_bin()
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("setup")
        .output()
        .unwrap();

    let output2 = slimbot_bin()
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("setup")
        .output()
        .unwrap();

    // Second run should also succeed (idempotent)
    assert!(output2.status.success());
}

// ── Log CLI arguments ──

#[test]
fn test_log_flag_accepted() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");

    let output = slimbot_bin()
        .arg("--log=0")
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("setup")
        .output()
        .unwrap();

    assert!(output.status.success());
}

#[test]
fn test_log_file_flag_accepted() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let log_path = tmp.path().join("test.log");

    let output = slimbot_bin()
        .arg("--log=0")
        .arg(format!("--log-file={}", log_path.display()))
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("setup")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(log_path.exists());
}

// ── Agent subcommand (config validation) ──

#[test]
fn test_agent_requires_config() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");

    // No config file → should fail
    let output = slimbot_bin()
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("agent")
        .output()
        .unwrap();

    // Should fail because config.json doesn't exist
    assert!(!output.status.success());
}


// ── Data-dir and workspace-dir flags ──

#[test]
fn test_data_dir_flag_used_by_setup() {
    let tmp = TempDir::new().unwrap();
    let custom_data = tmp.path().join("custom_slimbot");

    let output = slimbot_bin()
        .arg(format!("--data-dir={}", custom_data.display()))
        .arg("setup")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(custom_data.exists());
    assert!(custom_data.join("workspace").exists());
}

#[test]
fn test_workspace_dir_must_be_under_data_dir() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let ws = tmp.path().join("outside");

    let output = slimbot_bin()
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg(format!("--workspace-dir={}", ws.display()))
        .arg("setup")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("must be under data_dir"));
}

// ── Global flags on subcommands ──

#[test]
fn test_log_flag_global_on_setup() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");

    // --log should work before setup subcommand
    let output = slimbot_bin()
        .arg("--log=3")
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("setup")
        .output()
        .unwrap();

    assert!(output.status.success());
}

#[test]
fn test_log_file_global_on_setup() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let log_path = tmp.path().join("setup.log");

    let output = slimbot_bin()
        .arg("--log-file")
        .arg(log_path.to_str().unwrap())
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("setup")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(log_path.exists());
}
