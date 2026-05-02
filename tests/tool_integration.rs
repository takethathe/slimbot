use slimbot::{ToolManager, create_tool, ensure_nonempty_tool_result, format_tool_error};

// ── ToolManager ──

#[tokio::test]
async fn test_tool_manager_register_and_execute() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_path_buf();
    let mut tm = ToolManager::new(workspace);

    // Register the shell tool manually
    let shell_tool = create_tool("shell", &tmp.path().join("ws")).unwrap();
    tm.register(shell_tool);

    let result = tm.execute("shell", serde_json::json!({"command": "echo hello"})).await.unwrap();
    assert!(result.contains("hello"));
}

#[tokio::test]
async fn test_tool_manager_execute_unknown_tool() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_path_buf();
    let tm = ToolManager::new(workspace);

    let result = tm.execute("nonexistent", serde_json::json!({})).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Tool not found"));
}

#[test]
fn test_tool_manager_to_openai_functions() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_path_buf();
    let mut tm = ToolManager::new(workspace);

    let shell_tool = create_tool("shell", &tmp.path().join("ws")).unwrap();
    tm.register(shell_tool);

    let defs = tm.to_openai_functions();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "shell");
    assert!(!defs[0].description.is_empty());
    assert!(defs[0].parameters.is_object());
}

// ── Built-in tool factory ──

#[test]
fn test_create_builtin_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();

    let tool_names = ["shell", "file_reader", "file_writer", "file_editor", "list_dir", "make_dir"];
    for name in &tool_names {
        let tool = create_tool(name, ws);
        assert!(tool.is_some(), "tool '{}' should be creatable", name);
    }

    // Unknown tool name returns None
    assert!(create_tool("nonexistent", ws).is_none());
}

// ── Individual tool behaviors ──

#[tokio::test]
async fn test_shell_tool_basic() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let shell_tool = create_tool("shell", &ws).unwrap();

    assert_eq!(shell_tool.name(), "shell");
    assert!(!shell_tool.description().is_empty());

    let result = shell_tool.execute(serde_json::json!({"command": "echo hello"})).await.unwrap();
    assert!(result.contains("hello"));
}

#[tokio::test]
async fn test_shell_tool_failed_command() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let shell_tool = create_tool("shell", &ws).unwrap();

    // Shell tool returns Ok even for failed commands, with exit code appended
    let result = shell_tool.execute(serde_json::json!({"command": "exit 1"})).await.unwrap();
    assert!(result.contains("Exit code: 1"));
}

#[tokio::test]
async fn test_file_writer_and_reader() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let writer = create_tool("file_writer", &ws).unwrap();
    let reader = create_tool("file_reader", &ws).unwrap();

    let test_file = "test.txt";
    let content = "Hello, integration test!";

    // Write
    writer.execute(serde_json::json!({
        "path": test_file,
        "content": content
    })).await.unwrap();

    // Read back
    let read_content = reader.execute(serde_json::json!({
        "path": test_file
    })).await.unwrap();
    assert!(read_content.contains(content));
}

#[tokio::test]
async fn test_list_dir_tool() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(ws.join("subdir")).unwrap();
    std::fs::write(ws.join("file.txt"), "x").unwrap();

    let list_dir = create_tool("list_dir", &ws).unwrap();
    let result = list_dir.execute(serde_json::json!({"path": "."})).await.unwrap();
    assert!(result.contains("file.txt"));
    assert!(result.contains("subdir"));
}

#[tokio::test]
async fn test_make_dir_tool() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let make_dir = create_tool("make_dir", &ws).unwrap();
    let new_dir = "new_subdir/nested";

    make_dir.execute(serde_json::json!({"path": new_dir})).await.unwrap();

    let created = ws.join(new_dir);
    assert!(created.is_dir());
}

// ── Tool result helpers ──

#[test]
fn test_ensure_nonempty_tool_result() {
    assert_eq!(
        ensure_nonempty_tool_result("mytool", ""),
        "(mytool completed with no output)"
    );
    assert_eq!(
        ensure_nonempty_tool_result("mytool", "   "),
        "(mytool completed with no output)"
    );
    assert_eq!(
        ensure_nonempty_tool_result("mytool", "actual result"),
        "actual result"
    );
}

#[test]
fn test_format_tool_error() {
    let formatted = format_tool_error("command not found");
    assert!(formatted.contains("Error:"));
    assert!(formatted.contains("command not found"));
    assert!(formatted.contains("Analyze the error above"));
}
