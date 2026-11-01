//! The secret resolution path must never write a value to the log.
//!
//! One test per binary: the logger is a process-wide `OnceLock`.

use std::fs;

use slimbot::secrets::{MapSource, SecretStore, SourceKind};
use slimbot::{Config, LogLevel, log_init};

#[test]
fn test_resolved_keychain_value_never_reaches_log_output() {
    const SECRET: &str = "sk-keychain-only-value";

    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.json");
    // The file carries neither value: both must be resolved from the chain.
    fs::write(
        &config_path,
        r#"{
            "agent": {"provider": "default"},
            "providers": {"default": {"type": "openai", "api_key": "", "model": "gpt-4o"}},
            "triggers": {"secrets": [{"name": "github", "value": ""}]}
        }"#,
    )
    .unwrap();

    let log_path = tmp.path().join("slimbot.log");
    log_init(LogLevel::Debug, Some(&log_path)).unwrap();

    let store = SecretStore::new(vec![(
        SourceKind::Keychain,
        Box::new(MapSource::new([
            ("slimbot.default", SECRET),
            ("slimbot.trigger.github", SECRET),
        ])),
    )]);

    let config = Config::load_resolved_with(config_path.to_str().unwrap(), &store).unwrap();
    assert_eq!(config.providers["default"].api_key, SECRET);
    assert_eq!(config.triggers.secrets[0].value, SECRET);

    let logs = fs::read_to_string(&log_path).unwrap();
    // The resolution itself is observable — by key and source, never by value.
    assert!(
        logs.contains("slimbot.default"),
        "resolution should be logged by key: {logs}"
    );
    assert!(
        !logs.contains(SECRET),
        "resolved secret leaked into the log: {logs}"
    );
}
