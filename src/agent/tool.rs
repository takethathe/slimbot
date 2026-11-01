//! The `subagent` tool: start a run and return immediately.
//!
//! Visibility is structural: the main loop gets this tool, agent runs never do
//! (they are rejected if they declare it), so recursion cannot happen and the
//! global concurrency cap cannot multiply.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;

use crate::tool::{Tool, current_turn_target};

use super::runtime::{AcquireMode, Origin, StartReject, StartRequest, SubagentRuntime};

pub struct SubagentTool {
    runtime: Arc<SubagentRuntime>,
}

impl SubagentTool {
    pub fn new(runtime: Arc<SubagentRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }

    /// Risk class (ADR 0011): Runs a nested agent; its own actions are gated separately with unattended semantics.
    fn risk_level(&self) -> crate::approval::RiskLevel {
        crate::approval::RiskLevel::WorkspaceWrite
    }

    fn description(&self) -> &str {
        "Start a subagent that runs one task in its own session with its own tools. Returns immediately with a run_id; the result arrives later as a [Subagent result] message."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Name of an agent listed in `# Available Agents`"
                },
                "task": {
                    "type": "string",
                    "description": "The full task for the subagent. It does not see this conversation, so include every piece of context it needs."
                }
            },
            "required": ["agent", "task"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let agent = args
            .get("agent")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if agent.is_empty() || task.is_empty() {
            return Ok(json!({
                "status": "rejected",
                "reason": "invalid_args",
                "message": "Both `agent` and `task` are required."
            })
            .to_string());
        }

        let (channel, chat_id) = current_turn_target();
        let origin = if channel.is_empty() || chat_id.is_empty() {
            None
        } else {
            Some(Origin::new(channel, chat_id))
        };

        let req = StartRequest {
            agent,
            task,
            origin,
            deliver: None,
        };

        match self.runtime.start(req, AcquireMode::RejectWhenFull).await {
            Ok(started) => Ok(json!({
                "status": "started",
                "agent": started.agent,
                "run_id": started.run_id,
                "delivers": started.delivers,
                "timeout_secs": started.timeout_secs,
                "message": "The subagent is running. Its result will arrive later as a [Subagent result] message — do not wait for it, and do not duplicate the work."
            })
            .to_string()),
            Err(reject) => Ok(reject_json(&reject)),
        }
    }
}

fn reject_json(reject: &StartReject) -> String {
    match reject {
        StartReject::Busy { current, max } => json!({
            "status": "rejected",
            "reason": "busy",
            "running": current,
            "max": max,
            "message": format!("{} subagent run(s) are already in flight (max {}). Mention it to the user or try again later.", current, max)
        }),
        StartReject::UnknownAgent { available } => json!({
            "status": "rejected",
            "reason": "unknown_agent",
            "available": available,
            "message": "No agent with that name. Pick one from `# Available Agents`."
        }),
        StartReject::MissingTools { missing } => json!({
            "status": "rejected",
            "reason": "missing_tools",
            "missing": missing,
            "available": crate::agent::known_tool_names(),
            "message": format!(
                "The agent definition declares tools that are not available: {}. Available tools: {}.",
                missing.join(", "),
                crate::agent::known_tool_names().join(", ")
            )
        }),
        StartReject::Internal(e) => json!({
            "status": "rejected",
            "reason": "internal",
            "message": format!("Could not start the subagent: {}", e)
        }),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reject_busy_carries_counts() {
        let msg = reject_json(&StartReject::Busy { current: 2, max: 3 });
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["reason"], "busy");
        assert_eq!(v["running"], 2);
        assert_eq!(v["max"], 3);
    }

    #[test]
    fn test_reject_unknown_agent_lists_available() {
        let msg = reject_json(&StartReject::UnknownAgent {
            available: vec!["trigger".into()],
        });
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["reason"], "unknown_agent");
        assert_eq!(v["available"][0], "trigger");
    }

    #[test]
    fn test_reject_missing_tools_echoes_full_tool_set() {
        let msg = reject_json(&StartReject::MissingTools {
            missing: vec!["nope".into()],
        });
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["reason"], "missing_tools");
        let available = v["available"].as_array().unwrap();
        assert!(available.iter().any(|t| t == "web_fetch"));
        assert!(v["message"].as_str().unwrap().contains("web_fetch"));
    }
}
