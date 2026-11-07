use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::task_local;

use crate::config::ToolEntry;
use crate::utils::{TOOL_RESULTS_DIR, build_persisted_reference};
use crate::{debug, error, warn_log};

// Re-export for runner.rs
pub use crate::utils::truncate_text_head_tail;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// Per-turn state scoped to a single `AgentRunner::run()` invocation.
/// Stored in a task-local so concurrent runs sharing one `Arc<ToolManager>`
/// each observe their own instance (no cross-run context clobbering).
pub struct TurnContext {
    channel: String,
    chat_id: String,
    sent_in_turn: AtomicBool,
    /// The run's nested tool-call authority (ticket 25). Present for every
    /// runner-built turn; `None` when the tool executes outside a run loop
    /// (unit tests, direct host calls), where codemode has nothing to gate.
    tool_exec: Option<Arc<crate::runner::gate::RunToolExec>>,
}

impl TurnContext {
    pub fn new(channel: String, chat_id: String) -> Self {
        Self {
            channel,
            chat_id,
            sent_in_turn: AtomicBool::new(false),
            tool_exec: None,
        }
    }

    /// Attach the run's tool-executor handle.
    pub(crate) fn with_tool_exec(mut self, exec: Arc<crate::runner::gate::RunToolExec>) -> Self {
        self.tool_exec = Some(exec);
        self
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    /// Mark that the message tool delivered output to the origin channel this turn.
    /// Only marks when `channel`/`chat_id` match this turn's origin, so
    /// cross-channel sends never suppress the final response.
    pub fn mark_sent_if_target(&self, channel: &str, chat_id: &str) {
        if channel == self.channel && chat_id == self.chat_id {
            self.sent_in_turn.store(true, Ordering::Release);
        }
    }

    pub fn sent_in_turn(&self) -> bool {
        self.sent_in_turn.load(Ordering::Acquire)
    }

    /// The run's nested tool-call authority, when this turn was built by a
    /// runner. Read through here when the caller already holds the turn: the
    /// task-local is then read once instead of twice.
    pub(crate) fn tool_exec(&self) -> Option<Arc<crate::runner::gate::RunToolExec>> {
        self.tool_exec.clone()
    }
}

task_local! {
    /// The turn context for the currently executing `AgentRunner::run()` invocation.
    static CURRENT_TURN: Arc<TurnContext>;
}

/// Open a turn context scope. The context is available to all tool executions
/// within `fut` and is automatically cleared when the future completes
/// (RAII via task-local scope).
pub async fn with_turn_context<R>(ctx: Arc<TurnContext>, fut: impl Future<Output = R>) -> R {
    CURRENT_TURN.scope(ctx, fut).await
}

/// Read the current turn context, if running inside a `with_turn_context` scope.
/// Returns `None` when called outside any turn (e.g. direct unit tests).
pub fn current_turn() -> Option<Arc<TurnContext>> {
    CURRENT_TURN.try_get().ok()
}

/// Resolve the origin (channel, chat_id) of the current turn, defaulting to
/// empty strings when no turn context is active. Tools that need the turn
/// itself (e.g. to mark delivery) should use `current_turn()` instead.
pub fn current_turn_target() -> (String, String) {
    match current_turn() {
        Some(t) => (t.channel().to_string(), t.chat_id().to_string()),
        None => (String::new(), String::new()),
    }
}

/// The current run's nested tool-call authority (ticket 25), or `None` when
/// outside a runner-built turn. The codemode tool needs this to execute
/// `tools.*` host calls under the run's own approval gate.
pub(crate) fn current_tool_exec() -> Option<Arc<crate::runner::gate::RunToolExec>> {
    current_turn().and_then(|turn| turn.tool_exec())
}

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;

    /// Whether this tool's results are exempt from context pressure.
    ///
    /// Exempt means "neither truncated to a length limit nor replaced by a
    /// placeholder when old": both stages read this single self-report. Tools
    /// that control their own output size (or whose loss would make the model
    /// repeat a user-visible action, e.g. delivering a message) report `true`.
    /// There is deliberately no central allowlist, so third-party (MCP) tools
    /// default to `false` and get trimmed aggressively.
    fn context_exempt(&self) -> bool {
        false
    }

    /// The tool's risk class, declared here and deliberately **not**
    /// configurable (ADR 0011): configuration picks a control level *per risk
    /// class*, so a new tool inherits the policy of the class it belongs to
    /// instead of shipping with its own approval switch.
    ///
    /// There is no default on purpose: every implementation must state what it
    /// can do to the host, the workspace, the network or a third party.
    fn risk_level(&self) -> crate::approval::RiskLevel;

    async fn execute(&self, args: serde_json::Value) -> Result<String>;
}

pub struct ToolManager {
    tools: HashMap<String, Box<dyn Tool>>,
    workspace_dir: PathBuf,
    /// Cached OpenAI tool definitions, rebuilt on register.
    openai_tools: Vec<ToolDefinition>,
}

impl ToolManager {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self {
            tools: HashMap::new(),
            workspace_dir,
            openai_tools: Vec::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        debug!("Registered tool: {}", name);
        self.openai_tools.push(ToolDefinition {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            parameters: tool.parameters(),
        });
        self.tools.insert(name, tool);
    }

    pub fn init_from_config(&mut self, entries: &[ToolEntry]) {
        // If no tools configured in config, enable all built-in tools by default
        let default_entries = vec![
            ToolEntry {
                name: "shell".into(),
                enabled: true,
            },
            ToolEntry {
                name: "file_reader".into(),
                enabled: true,
            },
            ToolEntry {
                name: "file_writer".into(),
                enabled: true,
            },
            ToolEntry {
                name: "file_editor".into(),
                enabled: true,
            },
            ToolEntry {
                name: "list_dir".into(),
                enabled: true,
            },
            ToolEntry {
                name: "make_dir".into(),
                enabled: true,
            },
            ToolEntry {
                name: "grep".into(),
                enabled: true,
            },
            ToolEntry {
                name: "find_files".into(),
                enabled: true,
            },
            ToolEntry {
                name: "web_fetch".into(),
                enabled: true,
            },
            ToolEntry {
                name: "codemode".into(),
                enabled: true,
            },
        ];
        let effective = if entries.is_empty() {
            &default_entries
        } else {
            entries
        };

        for entry in effective {
            if entry.enabled {
                if let Some(tool) = create_builtin_tool(&entry.name, &self.workspace_dir) {
                    self.register(tool);
                } else {
                    warn_log!("Unknown tool: {}", entry.name);
                }
            }
        }
    }

    pub fn to_openai_functions(&self) -> Vec<ToolDefinition> {
        self.openai_tools.clone()
    }

    pub async fn execute(&self, name: &str, args: serde_json::Value) -> Result<String> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Tool not found: {}", name))?;
        tool.execute(args).await
    }

    /// Whether the named tool reports itself exempt from context pressure.
    ///
    /// Unknown tools (never registered here, e.g. an MCP tool reached through
    /// codemode) are not exempt: their results are trimmed like any other.
    pub fn is_context_exempt(&self, name: &str) -> bool {
        self.tools
            .get(name)
            .map(|tool| tool.context_exempt())
            .unwrap_or(false)
    }

    /// The risk class declared by the named tool, or `None` when no such tool
    /// is registered.
    ///
    /// `None` means the call cannot run at all (`execute` fails with "Tool not
    /// found"), so there is nothing to authorize; the approval hook lets it
    /// through to fail on its own rather than asking the owner to approve a
    /// tool that does not exist. Any tool that *is* registered has declared its
    /// class on the trait, which is where the gate comes from.
    pub fn risk_level(&self, name: &str) -> Option<crate::approval::RiskLevel> {
        self.tools.get(name).map(|tool| tool.risk_level())
    }
}

fn create_builtin_tool(name: &str, workspace_dir: &Path) -> Option<Box<dyn Tool>> {
    crate::tools::create_tool(name, workspace_dir)
}

/// Replace semantically empty tool results with a short marker string.
pub fn ensure_nonempty_tool_result(tool_name: &str, content: &str) -> String {
    if content.is_empty() || content.trim().is_empty() {
        format!("({} completed with no output)", tool_name)
    } else {
        content.to_string()
    }
}

/// Format a tool execution error into a model-friendly message.
pub fn format_tool_error(error_msg: &str) -> String {
    format!(
        "Error: {}\n\n[Analyze the error above and try a different approach.]",
        error_msg,
    )
}

/// Persist oversized tool result to disk and return a reference string.
pub fn persist_tool_result(
    workspace_dir: &Path,
    tool_call_id: &str,
    content: &str,
    max_chars: usize,
    preview_chars: usize,
) -> String {
    if max_chars > 0 && content.len() <= max_chars {
        return content.to_string();
    }

    let Some(_rel_path) =
        crate::utils::persist_tool_result_file(workspace_dir, tool_call_id, content)
    else {
        error!("Failed to persist tool result for {}", tool_call_id);
        return content.to_string();
    };

    let file_path = workspace_dir
        .join(TOOL_RESULTS_DIR)
        .join(format!("{}.txt", tool_call_id));
    build_persisted_reference(&file_path, content, preview_chars)
}

/// Test helpers shared by tool tests across modules.
#[cfg(test)]
pub(crate) mod test_util {
    use super::*;

    /// Build a turn context for tests.
    pub(crate) fn turn_ctx(channel: &str, chat_id: &str) -> Arc<TurnContext> {
        Arc::new(TurnContext::new(channel.to_string(), chat_id.to_string()))
    }

    /// Run `fut` inside a fresh turn scope for tests.
    pub(crate) async fn with_turn<R>(
        channel: &str,
        chat_id: &str,
        fut: impl Future<Output = R>,
    ) -> R {
        with_turn_context(turn_ctx(channel, chat_id), fut).await
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::turn_ctx;
    use super::*;
    use crate::config::ToolEntry;

    #[tokio::test]
    async fn test_turn_context_available_inside_scope_and_cleared_after() {
        let ctx = turn_ctx("cli", "c1");
        assert!(current_turn().is_none(), "no context before scope");
        with_turn_context(ctx, async {
            assert_eq!(current_turn().unwrap().channel(), "cli");
            assert_eq!(current_turn().unwrap().chat_id(), "c1");
        })
        .await;
        assert!(current_turn().is_none(), "context cleared after scope");
    }

    #[tokio::test]
    async fn test_turn_context_concurrent_isolation() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
        let make = |channel: &'static str| {
            let seen = seen.clone();
            let ctx = turn_ctx(channel, "x");
            async move {
                with_turn_context(ctx, async {
                    // Yield so the other task runs within its own scope.
                    tokio::task::yield_now().await;
                    let c = current_turn().unwrap();
                    seen.lock()
                        .unwrap()
                        .push((channel.to_string(), c.channel().to_string()));
                })
                .await
            }
        };
        let h1 = tokio::spawn(make("cli"));
        let h2 = tokio::spawn(make("webui"));
        h1.await.unwrap();
        h2.await.unwrap();
        let got = seen.lock().unwrap();
        assert_eq!(got.len(), 2, "both tasks should have recorded");
        for (expected, actual) in got.iter() {
            assert_eq!(expected, actual, "each task must see its own context");
        }
    }

    #[test]
    fn test_turn_context_mark_sent_if_target() {
        let ctx = Arc::new(TurnContext::new("cli".into(), "c1".into()));
        assert!(!ctx.sent_in_turn(), "fresh turn starts false");
        ctx.mark_sent_if_target("webui", "c1");
        assert!(
            !ctx.sent_in_turn(),
            "cross-channel target must not mark the turn"
        );
        ctx.mark_sent_if_target("cli", "c1");
        assert!(ctx.sent_in_turn(), "origin target marks the turn");
    }

    #[test]
    fn test_current_turn_target_outside_scope_defaults_to_empty() {
        assert!(current_turn().is_none());
        assert_eq!(current_turn_target(), (String::new(), String::new()));
    }

    #[test]
    fn test_tool_manager_new_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let tm = ToolManager::new(tmp.path().to_path_buf());
        assert!(tm.to_openai_functions().is_empty());
    }

    #[test]
    fn test_tool_manager_register_and_execute() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tm = ToolManager::new(tmp.path().to_path_buf());
        tm.register(Box::new(crate::tools::shell::ShellTool::default()));

        let funcs = tm.to_openai_functions();
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "shell");
        assert!(!funcs[0].description.is_empty());
    }

    #[test]
    fn test_tool_manager_execute_unknown_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let tm = ToolManager::new(tmp.path().to_path_buf());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(tm.execute("nonexistent", serde_json::json!({})));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Tool not found"));
    }

    #[test]
    fn test_tool_manager_init_from_config_all_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tm = ToolManager::new(tmp.path().to_path_buf());
        tm.init_from_config(&[]); // empty config → all defaults

        let funcs = tm.to_openai_functions();
        assert!(!funcs.is_empty());
        // All 9 built-in tools should be registered
        let names: Vec<_> = funcs.iter().map(|f| &f.name).collect();
        assert!(names.contains(&&"shell".to_string()));
        assert!(names.contains(&&"file_reader".to_string()));
        assert!(names.contains(&&"file_writer".to_string()));
        assert!(names.contains(&&"file_editor".to_string()));
        assert!(names.contains(&&"list_dir".to_string()));
        assert!(names.contains(&&"make_dir".to_string()));
        assert!(names.contains(&&"grep".to_string()));
        assert!(names.contains(&&"find_files".to_string()));
        assert!(names.contains(&&"web_fetch".to_string()));
    }

    #[test]
    fn test_tool_manager_init_from_config_selective() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tm = ToolManager::new(tmp.path().to_path_buf());
        tm.init_from_config(&[
            ToolEntry {
                name: "shell".into(),
                enabled: true,
            },
            ToolEntry {
                name: "file_reader".into(),
                enabled: false,
            },
        ]);

        let funcs = tm.to_openai_functions();
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "shell");
    }

    #[test]
    fn test_tool_manager_init_from_config_unknown_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tm = ToolManager::new(tmp.path().to_path_buf());
        tm.init_from_config(&[ToolEntry {
            name: "nonexistent_tool".into(),
            enabled: true,
        }]);

        // Should be empty since unknown tool is skipped
        assert!(tm.to_openai_functions().is_empty());
    }

    #[test]
    fn test_ensure_nonempty_tool_result() {
        assert_eq!(
            ensure_nonempty_tool_result("test_tool", ""),
            "(test_tool completed with no output)"
        );
        assert_eq!(
            ensure_nonempty_tool_result("test_tool", "   "),
            "(test_tool completed with no output)"
        );
        assert_eq!(ensure_nonempty_tool_result("test_tool", "hello"), "hello");
    }

    #[test]
    fn test_format_tool_error() {
        let formatted = format_tool_error("file not found");
        assert!(formatted.contains("file not found"));
        assert!(formatted.contains("Analyze the error"));
    }

    #[test]
    fn test_persist_tool_result_under_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let result = persist_tool_result(tmp.path(), "call-1", "short result", 100, 20);
        assert_eq!(result, "short result");
    }

    #[test]
    fn test_persist_tool_result_over_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let content = "x".repeat(2000); // well over the 1200 preview limit
        let result = persist_tool_result(tmp.path(), "call-2", &content, 100, 1200);
        assert!(result.contains("persisted"));
        assert!(result.contains("call-2"));
        assert!(result.contains("Original size"));
    }

    #[test]
    fn test_persist_tool_result_honours_preview_ratio() {
        let tmp = tempfile::tempdir().unwrap();
        let content = "y".repeat(5_000);
        let result = persist_tool_result(tmp.path(), "call-3", &content, 100, 300);
        // Header + a 300 char preview only: the preview limit is window-derived.
        assert!(result.contains(&"y".repeat(300)));
        assert!(!result.contains(&"y".repeat(301)));
    }

    #[test]
    fn test_persist_tool_result_writes_file_under_tool_results_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let content = "z".repeat(5_000);
        persist_tool_result(tmp.path(), "call-4", &content, 100, 1200);
        let path = tmp.path().join("tool-results").join("call-4.txt");
        assert_eq!(std::fs::read_to_string(path).unwrap(), content);
    }

    #[test]
    fn test_tool_context_exempt_defaults_to_false() {
        struct Plain;

        #[async_trait]
        impl Tool for Plain {
            fn name(&self) -> &str {
                "plain"
            }
            fn description(&self) -> &str {
                "plain"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn risk_level(&self) -> crate::approval::RiskLevel {
                crate::approval::RiskLevel::ReadOnly
            }
            async fn execute(&self, _args: serde_json::Value) -> Result<String> {
                Ok(String::new())
            }
        }

        assert!(!Plain.context_exempt());
    }

    #[test]
    fn test_tool_manager_reports_self_declared_exemption() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tm = ToolManager::new(tmp.path().to_path_buf());
        tm.init_from_config(&[]);

        assert!(
            tm.is_context_exempt("file_reader"),
            "file_reader keeps its own output in bounds"
        );
        assert!(
            !tm.is_context_exempt("shell"),
            "shell output is trimmed under pressure"
        );
        assert!(
            !tm.is_context_exempt("never_registered"),
            "unknown tools (e.g. MCP) are not exempt by default"
        );
    }

    #[test]
    fn test_create_builtin_tool_known_names() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(create_builtin_tool("shell", tmp.path()).is_some());
        assert!(create_builtin_tool("file_reader", tmp.path()).is_some());
        assert!(create_builtin_tool("file_writer", tmp.path()).is_some());
        assert!(create_builtin_tool("file_editor", tmp.path()).is_some());
        assert!(create_builtin_tool("list_dir", tmp.path()).is_some());
        assert!(create_builtin_tool("make_dir", tmp.path()).is_some());
        assert!(create_builtin_tool("grep", tmp.path()).is_some());
        assert!(create_builtin_tool("find_files", tmp.path()).is_some());
        assert!(create_builtin_tool("web_fetch", tmp.path()).is_some());
        assert!(create_builtin_tool("unknown_tool", tmp.path()).is_none());
    }
}
