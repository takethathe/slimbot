//! The model-facing codemode entry (ticket 25).
//!
//! One tool, one script: the model submits an async `main(ctx)` program, the
//! sandbox runs it, and the JSON return value comes back as the tool result.
//! The script's only reach is the agent-entry capability set (see
//! [`crate::codemode::agent`]) — `ctx.tools.<name>()` for whatever the run
//! assembled and `ctx.searchTools()` to find names — and every nested call
//! passes the same approval gate a top-level tool call passes (ADR 0011).

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::codemode::agent::AgentHost;
use crate::codemode::host::HostEnv;
use crate::codemode::sandbox::{ENTRY_AGENT, Limits, RunRequest, execute_async};
use crate::tool::Tool;

pub struct CodemodeTool;

#[async_trait]
impl Tool for CodemodeTool {
    fn name(&self) -> &str {
        "codemode"
    }

    fn description(&self) -> &str {
        "Run a JavaScript program inside the sandbox. Define exactly one entry: \
         `async function main(ctx)`. The return value must be JSON-serializable and \
         comes back as this tool's result. Reachable capabilities: \
         `await ctx.tools.<name>(args)` calls one of this run's tools (each call goes \
         through the normal approval gate: a DENIED or NOT EXECUTED answer means the \
         tool did not run — continue without it or tell the owner); \
         `await ctx.searchTools(query, limit?)` returns a JSON string of ranked \
         `{name, description}` tools; `ctx.log(msg)`, `ctx.now()`, `ctx.boot_at()`, \
         `ctx.uptime_s()`. Nothing else exists: no require/import, no process, no \
         timers, no fetch, no filesystem. Prefer one codemode program over many \
         sequential tool calls when results need filtering, aggregation or fan-out."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "JavaScript source defining async function main(ctx)."
                },
                "ctx": {
                    "type": "object",
                    "description": "Optional plain data merged into the ctx argument."
                }
            },
            "required": ["script"]
        })
    }

    fn risk_level(&self) -> crate::approval::RiskLevel {
        // The carrier itself only computes: everything that touches the outside
        // world is a nested `tools.*` call, gated per call by its own risk class.
        crate::approval::RiskLevel::ReadOnly
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let script = args
            .get("script")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("`script` (string) is required"))?;
        let ctx = args.get("ctx").cloned().unwrap_or_else(|| json!({}));

        // Nested tool calls need the run's gate context; outside a runner turn
        // (unit tests, direct host calls) there is nothing to authorize against.
        let Some(exec) = crate::tool::current_tool_exec() else {
            return Err(anyhow::anyhow!(
                "codemode must run inside an agent turn: no tool executor is bound \
                 to this context, so tools.* would have nothing to gate."
            ));
        };

        let host = std::sync::Arc::new(AgentHost::new(exec.clone(), crate::tool::current_turn()));
        let request = RunRequest::new(script, ENTRY_AGENT)
            .with_ctx(ctx)
            .with_limits(Limits::agent());
        let outcome = execute_async(request, host.clone(), HostEnv::default()).await;

        let mut out = match outcome {
            Ok(value) => serde_json::to_string_pretty(&value)?,
            Err(e) => {
                // Script failures still deserve the script's own log lines.
                let logs = host.drain_logs();
                let mut text = format!("Error [{}]: {}", e.code(), e.message);
                if !logs.is_empty() {
                    text.push_str("\n[script logs]\n");
                    text.push_str(&logs.join("\n"));
                }
                return Err(anyhow::anyhow!(text));
            }
        };

        let logs = host.drain_logs();
        if !logs.is_empty() {
            out.push_str("\n[script logs]\n");
            out.push_str(&logs.join("\n"));
        }
        if exec.took_pending() {
            // Same owner-facing list the top-level gate ends a turn with
            // (ADR 0011): the script stopped at guidance, the owner decides.
            out.push('\n');
            out.push_str(&exec.pending_list_text());
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::approval::{ApprovalConfig, ApprovalStore, RiskLevel};
    use crate::runner::gate::{GateContext, RunToolExec};
    use crate::session::TaskHook;
    use crate::tool::{ToolManager, TurnContext, with_turn_context};

    struct EchoTool {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo_probe"
        }
        fn description(&self) -> &str {
            "echo back"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn risk_level(&self) -> RiskLevel {
            RiskLevel::ReadOnly
        }
        async fn execute(&self, _args: Value) -> Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("echoed".to_string())
        }
    }

    fn exec() -> (Arc<RunToolExec>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut manager = ToolManager::new(PathBuf::from("/tmp/slimbot-codemode-tool-test"));
        manager.register(Box::new(EchoTool {
            calls: calls.clone(),
        }));
        let exec = Arc::new(RunToolExec::new(GateContext {
            session_id: "cli:c1".to_string(),
            task_hook: TaskHook::new("cli:c1"),
            tool_manager: Arc::new(manager),
            approvals: Arc::new(ApprovalStore::new(ApprovalConfig::default())),
            connectors: None,
            tracer: None,
            cancel_token: None,
        }));
        (exec, calls)
    }

    #[tokio::test]
    async fn test_codemode_tool_runs_script_and_returns_result() {
        let (exec, calls) = exec();
        let turn = Arc::new(TurnContext::new("cli".into(), "c1".into()).with_tool_exec(exec));
        let tool = CodemodeTool;
        let result = with_turn_context(turn, async {
            tool.execute(json!({
                "script": "async function main(ctx) { \
                    const out = await ctx.tools.echo_probe({}); \
                    ctx.log('ran once'); \
                    return { out, twice: [out, out].length }; }"
            }))
            .await
        })
        .await
        .unwrap();
        assert!(result.contains("echoed"), "{result}");
        assert!(result.contains("[script logs]"), "{result}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_codemode_tool_outside_run_context_fails_loudly() {
        let tool = CodemodeTool;
        let err = tool
            .execute(json!({"script": "async function main(ctx) { return 1; }"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("inside an agent turn"), "{err}");
    }

    #[tokio::test]
    async fn test_codemode_tool_reports_script_error_with_code() {
        let (exec, _) = exec();
        let turn = Arc::new(TurnContext::new("cli".into(), "c1".into()).with_tool_exec(exec));
        let tool = CodemodeTool;
        let result = with_turn_context(turn, async {
            tool.execute(json!({"script": "const nope = 1;"})).await
        });
        // No main() defined: the sandbox reports a coded, catchable failure.
        let err = result.await.unwrap_err().to_string();
        assert!(err.contains("missing_entry"), "{err}");
    }
}
