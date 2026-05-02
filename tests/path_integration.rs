use std::fs;

use tempfile::TempDir;

use slimbot::PathManager;

// ── Path resolution defaults ──

#[test]
fn test_resolve_defaults_all_none() {
    let pm = PathManager::resolve(None, None, None).unwrap();
    assert!(pm.config_path().ends_with("config.json"));
    assert!(pm.data_dir().ends_with(".slimbot"));
    assert!(pm.workspace_dir().ends_with("workspace"));
}

#[test]
fn test_resolve_custom_data_dir_derives_workspace() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("mydata");
    let pm = PathManager::resolve(None, Some(data.to_str().unwrap()), None).unwrap();
    assert!(pm.data_dir().ends_with("mydata"));
    assert!(pm.workspace_dir().ends_with("mydata/workspace"));
}

#[test]
fn test_resolve_custom_workspace_under_data() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    let ws = data.join("custom_ws");
    let pm = PathManager::resolve(
        None,
        Some(data.to_str().unwrap()),
        Some(ws.to_str().unwrap()),
    )
    .unwrap();
    assert!(pm.workspace_dir().ends_with("data/custom_ws"));
}

#[test]
fn test_resolve_workspace_outside_data_fails() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    let ws = tmp.path().join("outside_ws");

    let result = PathManager::resolve(
        None,
        Some(data.to_str().unwrap()),
        Some(ws.to_str().unwrap()),
    );
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("must be under data_dir"));
}

// ── Config path resolution ──

#[test]
fn test_resolve_config_not_found_errors() {
    let result = PathManager::resolve(Some("/nonexistent/config.json"), None, None);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Config file not found"));
}

#[test]
fn test_resolve_config_existing() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    let config_file = data.join("config.json");
    fs::create_dir_all(&data).unwrap();
    fs::write(&config_file, "{}").unwrap();

    let pm = PathManager::resolve(
        Some(config_file.to_str().unwrap()),
        Some(data.to_str().unwrap()),
        None,
    )
    .unwrap();
    assert!(pm.config_path().ends_with("data/config.json"));
}

#[test]
fn test_resolve_config_defaults_to_data_dir() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("mydata");
    let pm = PathManager::resolve(None, Some(data.to_str().unwrap()), None).unwrap();
    // Config defaults to {data_dir}/config.json, file doesn't need to exist for default path
    assert!(pm.config_path().ends_with("mydata/config.json"));
}

// ── Sub-directory helpers ──

#[test]
fn test_session_dir() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    let pm = PathManager::resolve(None, Some(data.to_str().unwrap()), None).unwrap();
    assert!(pm.session_dir().ends_with("sessions"));
}

#[test]
fn test_skills_dir() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    let pm = PathManager::resolve(None, Some(data.to_str().unwrap()), None).unwrap();
    assert!(pm.skills_dir().ends_with("skills"));
}

#[test]
fn test_memory_dir() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    let pm = PathManager::resolve(None, Some(data.to_str().unwrap()), None).unwrap();
    assert!(pm.memory_dir().ends_with("memory"));
}

#[test]
fn test_tool_results_dir() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    let pm = PathManager::resolve(None, Some(data.to_str().unwrap()), None).unwrap();
    assert!(pm.tool_results_dir().ends_with(".tool_results"));
}

#[test]
fn test_bootstrap_file_path() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    let pm = PathManager::resolve(None, Some(data.to_str().unwrap()), None).unwrap();
    assert!(pm.bootstrap_file("SOUL.md").ends_with("workspace/SOUL.md"));
    assert!(pm.bootstrap_file("AGENTS.md").ends_with("workspace/AGENTS.md"));
}

// ── Sandbox validation ──

#[test]
fn test_validate_path_sandbox_allows_subpath() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    fs::create_dir_all(&ws).unwrap();

    let pm = PathManager::resolve(None, Some(ws.to_str().unwrap()), None).unwrap();

    let resolved = pm.validate_path_sandbox("subdir/file.txt").unwrap();
    assert!(resolved.starts_with(pm.workspace_dir()));
}

#[test]
fn test_validate_path_sandbox_rejects_escape() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    fs::create_dir_all(&ws).unwrap();

    let pm = PathManager::resolve(None, Some(ws.to_str().unwrap()), None).unwrap();

    let result = pm.validate_path_sandbox("../escape.txt");
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Path escapes workspace directory"));
}

#[test]
fn test_validate_path_sandbox_strips_leading_slash() {
    // validate_path_sandbox strips leading '/' and treats the path as relative
    // to the workspace directory — so absolute paths like "/etc/passwd" become
    // "etc/passwd" under the workspace.
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    fs::create_dir_all(&ws).unwrap();

    let pm = PathManager::resolve(None, Some(ws.to_str().unwrap()), None).unwrap();

    // Absolute path is normalized to workspace-relative
    let resolved = pm.validate_path_sandbox("/etc/passwd").unwrap();
    assert!(resolved.starts_with(pm.workspace_dir()));
    assert!(resolved.to_string_lossy().contains("etc/passwd"));
}

// ── Tilde expansion ──

#[test]
fn test_expand_home_tilde() {
    if let Some(home) = dirs::home_dir() {
        let expanded = slimbot::expand_home("~/.slimbot");
        assert_eq!(expanded, home.join(".slimbot"));

        let expanded = slimbot::expand_home("~");
        assert_eq!(expanded, home);

        let expanded = slimbot::expand_home("~/foo/bar");
        assert_eq!(expanded, home.join("foo/bar"));
    }
}

#[test]
fn test_expand_home_non_tilde() {
    assert_eq!(
        slimbot::expand_home("/absolute/path"),
        std::path::PathBuf::from("/absolute/path")
    );
    assert_eq!(
        slimbot::expand_home("relative/path"),
        std::path::PathBuf::from("relative/path")
    );
}
